//! Mirrors `packages/agent/src/harness/agent-harness.ts` — the `AgentHarness`
//! run loop and `AgentLane` trait.
//!
//! **Plan §2.3 refinement #2:** the TS `AgentHarness` is a stub shell (every
//! operation rejects with `HarnessNotImplemented`). The Rust port implements
//! the *real* run loop on top of [`rpi_agent::run_agent_loop`]. The TS file is
//! treated as the **type contract only**: the option shape, the `AgentLane`
//! interface, the outcome unions, and the defensive-copy contract (setters
//! clone inputs, getters return clones — mirroring TS `[...]`/`{...}`).
//!
//! What the v1 port implements (the rest is documented as deferred in
//! the initial port notes (retired)):
//! - [`AgentHarness::create`] — reject when the session already has records
//!   (TS `create.restore` is not implemented either; we surface restore as a
//!   `HarnessError::Io` rejection rather than a panic).
//! - `prompt` (text + message overloads), `skill`, `prompt_from_template` —
//!   drive `run_agent_loop` over the lane's branch path, persist each new
//!   `AgentMessage` via the [`Session`] facade, emit `RunStart`/`RunEnd` on the
//!   [`HarnessEventBus`], and write `operation_started`/`operation_finished`
//!   `LaneRecord`s. Pre-run compaction is evaluated (`should_compact` →
//!   `prepare_compaction` + `compact`) and the resulting `Compaction` entry is
//!   persisted before the run proceeds.
//! - `compact` (explicit), `abort`, `record_usage`, `wait_for_idle`,
//!   `run_when_idle`, and the full set of cloned get/set accessors.
//! - `AgentLane` trait + lane-bound runners. Existing session lanes execute
//!   against their own branch while sharing harness configuration and events.
//!
//! Outcome mapping: `run_agent_loop` returns `Result<NewMessages, AgentError>`.
//! The terminal assistant message's `stop_reason` decides
//! `Completed`/`Aborted`/`Failed`; `Deferred` + a `deferred` handle produces
//! `RunOutcome::Suspended` (the operation stays open until `resume`). All
//! rejections are `Result<T, HarnessError>`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use rpi_agent::message::AgentMessage;
use rpi_agent::{
    run_agent_loop, run_agent_loop_from_assistant, AfterToolCall, AfterToolResults, AgentContext,
    AgentEmitter, AgentEvent, AgentLoopConfig, AgentTool, BeforeToolCall, ConvertToLlm, QueueMode,
    ShouldStopAfterTurnContext, StreamFn, TransformContext,
};
use rpi_ai::types::{
    AssistantMessage, Content, DeferredHandle, StopReason, ThinkingLevel, Usage, UserContent,
    UserMessage,
};
use rpi_ai::{Model, Provider as AiProvider};

use crate::compaction::{
    compact, estimate_context_tokens, prepare_compaction, should_compact, CompactionLlmOptions,
};
use crate::error::{SessionError, SessionErrorCode};
use crate::events::{HarnessEvent, HarnessEventBus, RunEndEvent, RunEndOutcome, RunStartEvent};
use crate::messages::{
    convert_to_llm as harness_convert_to_llm,
    convert_to_llm_incremental as harness_convert_to_llm_incremental,
    create_compaction_summary_message,
};
use crate::prompt_templates::format_prompt_template_invocation;
use crate::result::{HarnessError, HarnessResult, OperationKind};
use crate::session::context::{
    build_session_context, build_session_context_owned, ContextEntryTransform,
    CustomEntryContextMessageProjector, SessionContextBuildOptions,
};
use crate::session::session::Session;
use crate::session::types::{
    AssistantFrameOp, AssistantFrameRecord, BranchBounds, CompactionReason, Entry, EntryCursor,
    EntryOrder, EntryQuery, JsonValue, LaneRecord, OperationError, OperationFinishedRecord,
    OperationIntent, OperationOutcome, OperationStartedRecord, ProvisionedEntry, ProvisionedKind,
    QueueCancelledRecord, QueueEnqueuedRecord, QueueKind, RecordBase, RecordQuery,
    RetryPendingRecord, SessionStats, SessionTree, StepAttemptRecord, StepKind, ToolReplay,
    ToolStartedRecord, UsageCause, UsageRecord, WriteDeferredRecord,
};
use crate::skills::{format_skill_command, format_skill_invocation, parse_skill_command};
use crate::system_prompt::compose_system_prompt;
use crate::types::{
    AgentHarnessOptions, AgentHarnessResources, AgentHarnessStreamOptions, CompactionSettings,
    DrivingMode, HarnessTool, HarnessToolExecution, MessageDelivery, PromptTemplate, RetryPolicy,
    SendMessageOptions, Skill,
};
use crate::watcher::{watch_listener, HarnessWatcher};

// ===========================================================================
// Outcomes — mirror the TS RunOutcome / CompactionOutcome / NavigationOutcome
// unions (agent-harness.ts:89-103). Each carries the lane + terminal ids.
// ===========================================================================

/// `RunOutcome` (harness-level). Mirrors TS `RunOutcome`. `leaf_id`/`final_entry_id`
/// name the lane's leaf + final persisted entry; `final_message` is the terminal
/// assistant message. `Suspended` carries the provider deferred handle.
#[derive(Debug, Clone)]
pub enum HarnessRunOutcome {
    Completed {
        leaf_id: String,
        final_entry_id: String,
        final_message: rpi_ai::types::AssistantMessage,
    },
    Aborted {
        leaf_id: String,
        final_entry_id: String,
        final_message: rpi_ai::types::AssistantMessage,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
        final_entry_id: Option<String>,
        final_message: Option<rpi_ai::types::AssistantMessage>,
    },
    Suspended {
        leaf_id: String,
        final_entry_id: String,
        deferred: DeferredHandle,
    },
}

/// `CompactionOutcome`. Mirrors TS `CompactionOutcome`.
#[derive(Debug, Clone)]
pub enum CompactionOutcome {
    Completed {
        leaf_id: String,
        entry: Entry,
    },
    Declined {
        leaf_id: String,
    },
    Aborted {
        leaf_id: String,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
    },
}

/// `NavigationOutcome`. Mirrors TS `NavigationOutcome`. v1 only emits
/// `Declined` on a no-op navigation (the real branch navigation loop is
/// deferred).
#[derive(Debug, Clone)]
pub enum NavigationOutcome {
    Completed {
        new_leaf_id: Option<String>,
        summary_entry: Option<Entry>,
    },
    Declined {
        leaf_id: Option<String>,
    },
    Aborted {
        leaf_id: Option<String>,
    },
    Failed {
        leaf_id: Option<String>,
        error: OperationError,
    },
}

/// Result of a `prompt`/`skill`/`prompt_from_template` call. Mirrors TS `RunResult`
/// = `Result<{ runId } & RunOutcome, RunRejected>`.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub run_id: String,
    pub outcome: HarnessRunOutcome,
}

/// Result of an explicit `compact`. Mirrors TS `CompactionResult`.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub run_id: String,
    pub outcome: CompactionOutcome,
}

/// Result of `navigate_tree`. Mirrors TS `NavigationResult`.
#[derive(Debug, Clone)]
pub struct NavigationResult {
    pub run_id: String,
    pub outcome: NavigationOutcome,
}

/// Result of `steer`/`follow_up`/`next_run`. Mirrors TS `QueueResult`.
#[derive(Debug, Clone)]
pub struct QueueResult {
    pub entry_id: String,
}

/// Outcome of sending a message. Queued entry ids are stable reservations;
/// their transcript entries become readable when the selected queue is drained.
#[derive(Debug, Clone)]
pub enum SendMessageResult {
    Appended { entry_id: String },
    Queued { entry_id: String, queue: QueueKind },
    Run(RunResult),
}

/// Result of `abort`. Mirrors TS `AbortResult`.
///
/// `steer` / `follow_up` are the queued messages that were **reclaimed** by
/// the abort, images included and per message.
///
/// Upstream used to leave these empty: its TUI calls `clearQueue()` *before*
/// `abort()`, and a plugin/remote caller cannot do that without a race — a
/// message enqueued between the two calls is drained into the transcript by the
/// loop *and* handed back, so a remote UI resends it and the model sees it
/// twice. Reclaiming inside the same write gate as the cancellation removes
/// that window.
#[derive(Debug, Clone)]
pub struct AbortResult {
    pub run_id: String,
    pub steer: Vec<AgentMessage>,
    pub follow_up: Vec<AgentMessage>,
}

/// Result of `record_usage`. Mirrors TS `RecordUsageResult` (`Result<void, Closed>`).
#[derive(Debug, Clone, Default)]
pub struct RecordUsageResult;

/// Result of `cancel_queued`. Mirrors TS `CancelQueuedResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelQueuedOutcome {
    Cancelled,
    AlreadyConsumed,
    AlreadyCleared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelQueuedResult {
    pub outcome: CancelQueuedOutcome,
}

/// A snapshot of the queued (not-yet-consumed) user messages on one lane,
/// split by queue kind. Mirrors upstream's `getSteeringMessages()` /
/// `getFollowUpMessages()` — the payload the pending-messages display renders
/// (`Steering: <text>` / `Follow-up: <text>`) and the `app.message.dequeue`
/// action restores into the editor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueuedMessages {
    pub steering: Vec<QueuedMessageView>,
    pub follow_up: Vec<QueuedMessageView>,
}

impl QueuedMessages {
    /// True when nothing is queued on the lane.
    pub fn is_empty(&self) -> bool {
        self.steering.is_empty() && self.follow_up.is_empty()
    }

    /// Every queued entry in restore order (steering first, then follow-up).
    pub fn iter(&self) -> impl Iterator<Item = &QueuedMessageView> {
        self.steering.iter().chain(self.follow_up.iter())
    }

    /// The queued texts, attachments dropped.
    ///
    /// For callers that only render a single-line pending row (`Steering: …`).
    /// Anything that has to restore a draft must use [`Self::iter`] instead —
    /// flattening loses which image belonged to which message.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.iter().map(|message| message.text.as_str())
    }
}

/// One queued message, with the images that belong to **it**.
///
/// The pairing matters: a queue can hold several messages, each with its own
/// attachments. Collecting every image into one flat list would lose which
/// picture went with which text — a restored remote draft would then attach
/// the wrong image (or attach it to the wrong message).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueuedMessageView {
    /// The user's text (image blocks excluded; they are in `images`).
    pub text: String,
    /// Image attachments of this specific message.
    pub images: Vec<rpi_ai::types::ImageContent>,
}

impl QueuedMessageView {
    /// True when the entry carries neither text nor images (nothing to restore).
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }

    /// Convert back to an [`AgentMessage`], preserving text **and** images.
    ///
    /// A message with images becomes `UserContent::Blocks` (text block first,
    /// then the images); a text-only message stays `UserContent::Text` so it
    /// round-trips with what the user typed.
    pub fn into_message(self) -> AgentMessage {
        use rpi_ai::types::{Content, TextContent, TextContentType, UserContent};
        let content = if self.images.is_empty() {
            UserContent::Text(self.text)
        } else {
            let mut blocks: Vec<Content> = Vec::with_capacity(self.images.len() + 1);
            if !self.text.is_empty() {
                blocks.push(Content::Text(TextContent {
                    kind: TextContentType,
                    text: self.text,
                    text_signature: None,
                }));
            }
            blocks.extend(self.images.into_iter().map(Content::Image));
            UserContent::Blocks(blocks)
        };
        AgentMessage::User(rpi_ai::types::UserMessage::new(content, 0))
    }
}

/// The display text of a queued message. Steer / follow-up entries are always
/// user content, so this is the text the user typed (image blocks are ignored,
/// matching the single-line pending row).
fn queued_message_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(user) => match &user.content {
            rpi_ai::types::UserContent::Text(text) => text.clone(),
            rpi_ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    rpi_ai::types::Content::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        _ => String::new(),
    }
}

/// A queued message as text **plus the images that belong to it**.
///
/// Deliberately one view per message rather than `texts: Vec<String>` +
/// `images: Vec<ImageContent>`: two queued messages can each carry a different
/// picture, and a flat image list cannot express which is which.
fn queued_message_view(message: &AgentMessage) -> QueuedMessageView {
    let mut images = Vec::new();
    if let AgentMessage::User(user) = message {
        if let rpi_ai::types::UserContent::Blocks(blocks) = &user.content {
            images.extend(blocks.iter().filter_map(|block| match block {
                rpi_ai::types::Content::Image(image) => Some(image.clone()),
                _ => None,
            }));
        }
    }
    QueuedMessageView {
        text: queued_message_text(message),
        images,
    }
}

// ===========================================================================
// Deferred-operation + inspection read models (TS `runtime/` surface). These
// back the `AgentHarness` methods `drive`/`resume`/`inspectExecution`/
// `getResult`/`snapshot`, which the coding-agent product layer consumes.
// ===========================================================================

/// Terminal/disposition state after driving a durable operation.
#[derive(Debug, Clone)]
pub enum DriveOutcome {
    /// The operation ran to a terminal run outcome.
    Completed(Box<RunResult>),
    /// The operation parked again on a fresh deferred handle.
    Suspended(DeferredHandle),
    /// There was nothing pending to drive on this lane.
    Idle,
}

/// Result of `drive(operationId)`. Mirrors the TS runtime drive result.
#[derive(Debug, Clone)]
pub struct DriveResult {
    pub operation_id: String,
    pub outcome: DriveOutcome,
}

/// The active-operation half of a [`LaneSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveOperationSnapshot {
    pub run_id: String,
    pub lane: String,
    pub kind: OperationKind,
}

/// A point-in-time read model of one lane. Mirrors the TS lane snapshot the
/// runtime/TUI renders from: the leaf pointer, the session name, rolled-up
/// stats, and (when a run is in flight) the active operation.
#[derive(Debug, Clone, Default)]
pub struct LaneSnapshot {
    pub lane: String,
    pub leaf_id: Option<String>,
    pub name: Option<String>,
    pub stats: Option<SessionStats>,
    pub active: Option<ActiveOperationSnapshot>,
    /// Labels keyed by target entry id (mirrors the TS `labels` fact map).
    pub labels: std::collections::BTreeMap<String, String>,
}

/// A tool-execution record surfaced by `inspect_execution`. Unlike the live
/// `ToolExecutionComponent` (a UI concern), this is the durable view: which
/// entry persisted the call, and whether a result landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionInfo {
    pub tool_call_id: String,
    pub tool_name: String,
    /// The entry id the tool call was persisted under, when known.
    pub entry_id: Option<String>,
    /// The entry id of the matching tool result, when one exists.
    pub result_entry_id: Option<String>,
}

// ===========================================================================
// AgentLane trait — the per-lane surface (TS AgentLane interface, lines 271-303)
// ===========================================================================

/// Per-lane operations. Mirrors TS `AgentLane`. [`AgentHarness`] (the main lane)
/// implements the run loop; [`LaneHandle`] (non-main lanes) delegate reads to a
/// [`SessionTree`] view and reject run ops with [`HarnessError::InvalidLane`].
#[async_trait::async_trait]
pub trait AgentLane: Send + Sync {
    fn name(&self) -> &str;
    async fn get_leaf_id(&self) -> HarnessResult<Option<String>>;

    /// Whether recovery left an interrupted run that should be continued.
    ///
    /// The caller (the TUI) decides *when* to continue: driving a run has to
    /// happen somewhere the output is rendered, and the harness cannot know that.
    async fn has_pending_resume(&self) -> bool;

    /// Continue the interrupted run, if there is one to continue.
    ///
    /// `Ok(None)` means there was nothing to resume; the caller should carry on
    /// with a normal prompt.
    async fn resume_pending(&self) -> HarnessResult<Option<RunResult>>;

    async fn prompt_text(
        &self,
        text: &str,
        images: Vec<rpi_ai::types::ImageContent>,
    ) -> HarnessResult<RunResult>;
    async fn prompt_message(&self, message: AgentMessage) -> HarnessResult<RunResult>;
    /// Send using Pi's `triggerTurn`/`deliverAs` semantics. Active-run deliveries
    /// enqueue immediately rather than waiting for the executing tool to finish.
    async fn send_message(
        &self,
        message: AgentMessage,
        options: SendMessageOptions,
    ) -> HarnessResult<SendMessageResult>;
    async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> HarnessResult<RunResult>;
    async fn skill(
        &self,
        name: &str,
        additional_instructions: Option<&str>,
    ) -> HarnessResult<RunResult>;
    async fn prompt_from_template(&self, name: &str, args: &[String]) -> HarnessResult<RunResult>;

    async fn compact(&self, custom_instructions: Option<&str>) -> HarnessResult<CompactionResult>;
    async fn navigate_tree(
        &self,
        target_id: Option<&str>,
        summarize: bool,
        custom_instructions: Option<&str>,
        label: Option<&str>,
    ) -> HarnessResult<NavigationResult>;

    async fn abort(&self) -> HarnessResult<AbortResult>;

    async fn steer(&self, message: AgentMessage) -> HarnessResult<QueueResult>;
    async fn follow_up(&self, message: AgentMessage) -> HarnessResult<QueueResult>;
    async fn next_run(&self, message: AgentMessage) -> HarnessResult<QueueResult>;
    async fn cancel_queued(&self, entry_id: &str) -> HarnessResult<CancelQueuedResult>;

    /// Snapshot the lane's queued (not-yet-consumed) steering + follow-up user
    /// messages. Read-only; drives the pending-messages display. Mirrors native
    /// pi's `getSteeringMessages()` / `getFollowUpMessages()`.
    async fn queued_messages(&self) -> HarnessResult<QueuedMessages>;

    /// Remove and return every queued steering + follow-up message on the lane.
    /// Mirrors upstream's `clearQueue()` (the `app.message.dequeue` action
    /// restores the returned text into the editor).
    async fn clear_queue(&self) -> HarnessResult<QueuedMessages>;
    async fn record_usage(
        &self,
        usage: Usage,
        entry_id: Option<&str>,
        details: Option<JsonValue>,
    ) -> HarnessResult<RecordUsageResult>;

    async fn wait_for_idle(&self) -> HarnessResult<()>;

    /// Wait for every in-flight durable write to settle.
    ///
    /// Called at the end of each run so the window in which a forced exit could
    /// lose the turn the user just watched is one turn wide instead of the whole
    /// session. Backends with nothing to wait for are a no-op.
    async fn drain_session(&self) {}
    /// Take the session writes that failed during this lane's runs.
    ///
    /// The host renders them: a warning inside an alternate screen reaches
    /// nobody, and a conversation that silently stopped being saved is the one
    /// failure a user has to act on before exiting.
    fn take_write_failures(&self) -> Vec<HarnessWriteFailure> {
        Vec::new()
    }
    async fn run_when_idle(
        &self,
        callback: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> HarnessResult<()>;

    async fn get_model(&self) -> HarnessResult<Model>;
    async fn set_model(&self, model: Model) -> HarnessResult<()>;
    async fn get_thinking_level(&self) -> HarnessResult<ThinkingLevel>;
    async fn set_thinking_level(&self, level: ThinkingLevel) -> HarnessResult<()>;
    async fn get_active_tools(&self) -> HarnessResult<Vec<String>>;
    async fn set_active_tools(&self, names: Vec<String>) -> HarnessResult<()>;

    fn session_view(&self) -> Arc<dyn SessionTree>;

    // -- durable-session read/write surface (TS `AgentLane` additions).
    //    Non-generic so the trait stays dyn-compatible; `watch` lives on the
    //    concrete `AgentHarness` because it is generic over the callback. ----

    async fn get_tip_id(&self) -> HarnessResult<Option<String>>;
    async fn find_entries(&self, query: &EntryQuery) -> HarnessResult<Vec<Entry>>;
    async fn find_entry(&self, query: &EntryQuery) -> HarnessResult<Option<Entry>>;
    async fn get_entry(&self, id: &str) -> HarnessResult<Option<Entry>>;
    async fn append_message(&self, message: AgentMessage) -> HarnessResult<String>;
    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> HarnessResult<String>;
    async fn get_name(&self) -> HarnessResult<Option<String>>;
    async fn set_name(&self, name: Option<&str>) -> HarnessResult<()>;
    async fn get_label(&self, target_id: &str) -> HarnessResult<Option<String>>;
    async fn set_label(&self, target_id: &str, label: Option<&str>) -> HarnessResult<()>;
    async fn get_stats(&self) -> HarnessResult<SessionStats>;
    async fn snapshot(&self) -> HarnessResult<LaneSnapshot>;
    /// Lightweight active-operation read. Custom lanes retain their snapshot
    /// semantics unless they provide an equivalent projection without labels.
    async fn active_operation(&self) -> HarnessResult<Option<ActiveOperationSnapshot>> {
        Ok(self.snapshot().await?.active)
    }
    async fn inspect_execution(
        &self,
        tool_call_id: &str,
    ) -> HarnessResult<Option<ToolExecutionInfo>>;
    async fn get_result(&self, run_id: &str) -> HarnessResult<Option<RunResult>>;
    async fn accept(&self, entry_id: &str) -> HarnessResult<bool>;
    async fn request_abort(&self, run_id: &str) -> HarnessResult<bool>;
    async fn drive(&self, operation_id: &str) -> HarnessResult<DriveResult>;
    async fn resume(&self, suspended_id: &str) -> HarnessResult<RunResult>;
}

// ===========================================================================
// HarnessInner - the shared mutable state behind the harness (model,
// thinking level, active tools, providers, run guard). Guarded by a Mutex so
// the AgentHarness (main lane) and LaneHandle (non-main) can share it.
// Defensive-copy: setters Clone, getters return clones.
// ===========================================================================

/// In-flight run guard. Held while a run/compaction operation is driving the
/// loop on `main`. The `signal` is a child of the harness root token; cancel it
/// to abort. `idle` fires when the guard is released.
struct ActiveRun {
    /// The durable operation ended, but its final callbacks still own admission.
    finishing: bool,
    /// Recovery can replay external tools, so it cannot hold message_writes.
    /// Scoped mutations are refused until those repairs have completed.
    recovering: bool,
    run_id: String,
    lane: String,
    signal: CancellationToken,
    /// Cancels delegated work on abort OR normal parent completion.
    children: CancellationToken,
    idle: Arc<tokio::sync::Notify>,
    kind: OperationKind,
}

/// A queued user message keeps its public entry id until the agent loop drains
/// it. The loop only needs the message, while the harness API also needs the id
/// for cancellation and UI feedback.
#[derive(Clone)]
struct QueuedMessage {
    entry_id: String,
    lane: String,
    message: AgentMessage,
}

struct MessageQueue {
    mode: QueueMode,
    pending: VecDeque<QueuedMessage>,
    reserved: std::collections::HashSet<String>,
}

type SharedMessageQueue = Arc<Mutex<MessageQueue>>;

impl MessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            mode,
            pending: VecDeque::new(),
            reserved: Default::default(),
        }
    }

    /// How many items are queued **for this lane**, without draining.
    ///
    /// Needed by the `pending_message_count` hook (the `turn_end`
    /// `pendingMessages` field): the drain helpers below consume the queue,
    /// so asking "how many are left?" with them would eat the messages.
    fn len_for_lane(&self, lane: &str) -> usize {
        self.pending.iter().filter(|item| item.lane == lane).count()
    }

    /// Take every queued item for `lane`, keeping each item's id.
    ///
    /// The id matters: it is what the durable `queue_enqueued` record named, so
    /// the caller can record the consumption against it. Dropping the ids here
    /// (as this used to) is what would make a consumed message come back to life
    /// on the next start.
    #[cfg(test)]
    fn drain_for_lane(&mut self, lane: &str) -> Vec<QueuedMessage> {
        let items: Vec<QueuedMessage> = match self.mode {
            QueueMode::All => {
                let mut selected = Vec::new();
                let mut retained = VecDeque::new();
                for item in self.pending.drain(..) {
                    if item.lane == lane {
                        selected.push(item);
                    } else {
                        retained.push_back(item);
                    }
                }
                self.pending = retained;
                selected
            }
            QueueMode::OneAtATime => {
                let Some(index) = self.pending.iter().position(|item| item.lane == lane) else {
                    return Vec::new();
                };
                self.pending.remove(index).into_iter().collect()
            }
        };
        items
    }

    /// Remove and return every queued item for `lane`, regardless of
    /// [`QueueMode`]. Used by the `app.message.dequeue` action (restore all
    /// queued messages into the editor), which also cancels them durably.
    #[cfg(test)]
    fn take_items_for_lane(&mut self, lane: &str) -> Vec<QueuedMessage> {
        let mut selected = Vec::new();
        let mut retained = VecDeque::new();
        for item in self.pending.drain(..) {
            if item.lane == lane {
                selected.push(item);
            } else {
                retained.push_back(item);
            }
        }
        self.pending = retained;
        selected
    }

    fn remove(&mut self, entry_id: &str) -> bool {
        let before = self.pending.len();
        self.pending.retain(|item| item.entry_id != entry_id);
        self.pending.len() != before
    }

    /// The queued message texts for `lane`, in queue order, without touching
    /// the queue. Used by the pending-messages display.
    fn peek_for_lane(&self, lane: &str) -> Vec<QueuedMessageView> {
        self.pending
            .iter()
            .filter(|item| item.lane == lane && !self.reserved.contains(&item.entry_id))
            .map(|item| queued_message_view(&item.message))
            .collect()
    }
}

/// Releases the in-memory active-run slot on every exit path, including an
/// early `?` caused by session I/O or configuration errors.
struct ActiveRunLease<'a> {
    harness: &'a AgentHarness,
    run_id: String,
}

impl Drop for ActiveRunLease<'_> {
    fn drop(&mut self) {
        self.harness.release_run(&self.run_id);
    }
}

/// A run that recovery repaired and that can be continued without the user
/// restating their request.
///
/// Upstream resumes the interrupted operation in place (`driveOperation`
/// re-enters at `state.at`). rpi's run has no durable re-entry point, so the
/// equivalent is to start a run with **no new prompt**: the branch already
/// ends with the resolved tool results, so the next thing the loop does is
/// exactly what the interrupted run would have done next — ask the provider
/// to continue.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingResume {
    /// The run that just died. Names the notice and the log entry.
    pub interrupted_run_id: String,
    /// The run the whole resume chain starts from. Every resumed run points back
    /// at this one, so the attempt count is a property of the chain rather than
    /// of whichever run happens to have died last.
    pub chain_origin_run_id: String,
    /// How many resumes this chain has already had. Bounds a run that keeps dying
    /// from restarting the process's work forever.
    pub attempt: u32,
}

/// How many times one interrupted run may be resumed automatically.
///
/// Without a bound, a run that crashes deterministically would be re-driven on
/// every startup: an unbounded loop that spends tokens and repeats side
/// effects while the user only ever sees a crash.
pub const MAX_RESUME_ATTEMPTS: u32 = 3;

/// Shared mutable state. Cloned out (field-by-field) at the start of each
/// operation so the loop sees a consistent snapshot; setters replace fields
/// in place.
struct HarnessInner {
    // The final harness owner cancels children even if weak runtime bindings
    // still retain the public session/event facades.
    child_scope: Arc<Mutex<CancellationToken>>,
    execution_scope: Option<Arc<rpi_agent::tool_execution_scope::ToolExecutionScope>>,
    skill_execution_env: Option<Arc<dyn rpi_tools::ExecutionEnv>>,
    model: Model,
    thinking_level: ThinkingLevel,
    active_tool_names: Vec<String>,
    exact_resources: bool,
    tools: Vec<HarnessTool>,
    resources: AgentHarnessResources,
    stream_options: AgentHarnessStreamOptions,
    retry: RetryPolicy,
    compaction: CompactionSettings,
    /// v1 leaves the harness in `Automatic` driving mode; the field is kept so
    /// the contract mirrors TS `AgentHarnessOptions.drive`.
    #[allow(dead_code)]
    driving: DrivingMode,
    tool_execution: HarnessToolExecution,
    system_prompt: Option<String>,
    /// Provider registry; the harness resolves by `model.provider`.
    models: Vec<Arc<dyn AiProvider>>,
    /// Optional caller-supplied converter; if `None` the harness-level
    /// `convert_to_llm` is used.
    to_provider_messages: Option<ConvertToLlm>,
    entry_projectors: BTreeMap<String, CustomEntryContextMessageProjector>,
    /// Caller-supplied context-entry transforms (B3b). Forwarded into
    /// `SessionContextBuildOptions.entry_transforms` at the `build_opts` site.
    entry_transforms: Vec<ContextEntryTransform>,
    /// Optional emitter override; see [`AgentHarnessOptions::agent_emitter`].
    agent_emitter: Option<Arc<dyn AgentEmitter>>,
    /// Set by `create` when recovery repaired a run that can be continued, and
    /// consumed by [`AgentHarness::resume_pending`]. See [`PendingResume`].
    pending_resume: Option<PendingResume>,
    // ---- B3b: the three exists-but-`None` AgentLoopConfig hooks -----------
    before_tool_call: Option<BeforeToolCall>,
    after_tool_call: Option<AfterToolCall>,
    transform_context: Option<TransformContext>,
    /// B4: per-call provider hooks fired inside the `StreamFn` closure.
    provider_hooks: Option<Arc<dyn rpi_ai::ProviderHooks>>,
    closed: bool,
    active_run: Option<ActiveRun>,
    steering_queue: SharedMessageQueue,
    follow_up_queue: SharedMessageQueue,
    next_run_queue: SharedMessageQueue,
    /// Fast path only; durable context enqueue records remain authoritative.
    pending_context_lanes: std::collections::HashSet<String>,
}

// ===========================================================================
// AgentHarness - a lane-bound runner. Drives run_agent_loop over its session
// branch path, persists entries/records, emits RunStart/RunEnd.
// ===========================================================================

/// The harness created by [`AgentHarness::create`] is bound to `"main"`.
/// [`AgentHarness::lane`] creates another lane-bound runner. Cheap to clone;
/// clones share the same session, event bus, and inner state.
#[derive(Clone)]
pub struct AgentHarness {
    session: Session,
    inner: Arc<Mutex<HarnessInner>>,
    bus: HarnessEventBus,
    lane: String,
    /// Serializes external appends with operation admission and boundary drains.
    /// Never held while executing a tool or invoking an external event handler.
    message_writes: Arc<tokio::sync::Mutex<()>>,
    /// Session writes that failed while the run continued. Drained by the host
    /// so the transcript can report them: a `tracing::warn!` inside an alternate
    /// screen reaches nobody, and a conversation that silently stopped being
    /// saved is the one failure a user has to act on before exiting.
    write_failures: Arc<Mutex<Vec<HarnessWriteFailure>>>,
    /// Messages whose append was refused, kept verbatim so `/export` can still
    /// write them. Session-backed export reads applied entries only, so without
    /// this the refused content would exist nowhere but the screen.
    unsaved_messages: Arc<Mutex<Vec<UnsavedMessage>>>,
    child_scope: Arc<Mutex<CancellationToken>>,
    is_child: bool,
    max_turns: Option<u32>,
    /// One-shot "finish this turn, then stop" request (runtime action
    /// `StopRound`).
    ///
    /// This is the gentle counterpart of abort: it does not interrupt the
    /// in-flight provider call or running tools, it only prevents the loop from
    /// starting another turn — which is what "stop after this round" means to a
    /// user who does not want the current answer truncated.
    ///
    /// Consumed at the next turn boundary, so it never leaks into the next prompt.
    stop_round: Arc<std::sync::atomic::AtomicBool>,
}

/// A session write that failed after the run had already decided to continue.
///
/// Carried to the host rather than logged: see `AgentHarness::take_write_failures`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessWriteFailure {
    /// What could not be persisted, e.g. "a salvaged assistant message".
    pub what: String,
    /// The backend's message, verbatim.
    pub message: String,
    /// When the failure was recorded (`Clock` milliseconds).
    pub at: i64,
}

/// An unsaved message kept as a recovery copy for the host's export path.
///
/// JSONL storage applies an entry to its in-memory view only after the append
/// succeeds, so a refused write leaves *no* copy of that message anywhere the
/// session-backed `/export` can read. Without this the guidance to export before
/// exiting would promise a recovery the export cannot deliver.
#[derive(Debug, Clone, PartialEq)]
pub struct UnsavedMessage {
    /// The message whose append was refused, verbatim.
    pub message: AgentMessage,
    /// What could not be persisted, matching the [`HarnessWriteFailure`] `what`.
    pub what: String,
    /// When the refusal was recorded (`Clock` milliseconds).
    pub at: i64,
    /// The durable session the refused message belonged to. A harness-wide
    /// vector outlives `set_session` switches, so a consumer that ignored this
    /// would export session A's recovery content as part of session B.
    pub session_id: String,
    /// The lane the refused message was written on.
    pub lane: String,
}

/// Record a failed session write, keeping the queue bounded.
///
/// Free-standing so the associated helpers that run before a harness exists (the
/// interrupted-operation recovery in `create`) can report the same way.
fn record_write_failure(queue: &Mutex<Vec<HarnessWriteFailure>>, what: &str, message: &str) {
    let mut failures = queue.lock().unwrap_or_else(|e| e.into_inner());
    if failures.len() >= MAX_WRITE_FAILURES {
        return;
    }
    failures.push(HarnessWriteFailure {
        what: what.to_string(),
        message: message.to_string(),
        at: now_ms(),
    });
}

/// Upper bound on queued write failures.
///
/// A backend that fails every write would otherwise grow this without limit for
/// as long as the run lasts. The host drains on every `AgentEnd` and at run end,
/// so the cap only accumulates while a single run fails repeatedly — 16 covers
/// that while keeping the report short enough to read.
pub const MAX_WRITE_FAILURES: usize = 16;

mod child;
pub use child::ChildHarness;
mod queue_writes;
pub(crate) use queue_writes::QueueReservation;
mod run_results;
mod scoped_entries;
pub use scoped_entries::{BranchWriteGuard, ScopedEntry};
mod weak;
pub use weak::WeakAgentHarness;

impl AgentHarness {
    /// Read-only startup gate for resource transitions. Application-level idle
    /// authorization remains the caller's responsibility.
    pub async fn validate_resource_transition_idle(session: &Session) -> HarnessResult<()> {
        if Self::resource_session_has_pending_work(session, None).await? {
            return Err(HarnessError::io(
                "resource transition rejects pending native execution/queue/recovery",
            ));
        }
        Ok(())
    }
    /// Read-only pending facts, including queue/recovery/resume. No harness
    /// creation, progress pruning, recovery writes or provider registration.
    pub async fn saved_session_has_pending_work(
        session: &Session,
        deadline: std::time::Instant,
    ) -> HarnessResult<bool> {
        Self::resource_session_has_pending_work(session, Some(deadline)).await
    }
    async fn resource_session_has_pending_work(
        session: &Session,
        deadline: Option<std::time::Instant>,
    ) -> HarnessResult<bool> {
        let runtime = crate::runtime::SessionRuntime::new(session.clone());
        for lane in session.get_lanes().await.map_err(session_to_harness_err)? {
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return Err(HarnessError::io("saved read processing deadline exceeded"));
            }
            let recovery = runtime
                .recover_lane(&lane.lane)
                .await
                .map_err(session_to_harness_err)?;
            if !recovery.is_clean()
                || !recovery.tool_frames.is_empty()
                || !Self::rebuild_queues(session, &lane.lane).await?.is_empty()
                || match deadline {
                    Some(at) => {
                        Self::lane_is_resumable_impl(session, &lane.lane, true, Some(at)).await?
                    }
                    None => Self::lane_is_resumable_checked(session, &lane.lane).await?,
                }
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Cancel the lane's run **and** reclaim its queues in one critical
    /// section.
    ///
    /// `idle_is_ok = false` keeps [`AgentLane::abort`]'s upstream contract: an
    /// idle lane is `NoActiveRun` (callers branch on it to mean "nothing to
    /// cancel"). `idle_is_ok = true` is for a plugin/remote "stop" button,
    /// which must be idempotent and must still hand back messages that landed
    /// as the run settled — erroring there would strand them.
    ///
    /// Either way the queue is reclaimed under the same `message_writes` gate as
    /// the cancellation, so a new run cannot start and drain the queue in
    /// between (which is exactly the race a separate `clear_queue()` call
    /// leaves open).
    pub async fn abort_reclaiming(&self, idle_is_ok: bool) -> HarnessResult<AbortResult> {
        let gate = self.message_writes.clone().lock_owned().await;
        let harness = self.clone();
        tokio::spawn(async move {
            let _gate = gate;
            harness.abort_reclaiming_locked(idle_is_ok).await
        })
        .await
        .map_err(|error| HarnessError::io(format!("queue abort task failed: {error}")))?
    }

    async fn abort_reclaiming_locked(&self, idle_is_ok: bool) -> HarnessResult<AbortResult> {
        let run_id = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            match &inner.active_run {
                Some(active) if active.lane == self.lane => {
                    active.signal.cancel();
                    Some(active.run_id.clone())
                }
                Some(_) => {
                    return Err(HarnessError::no_active_run(
                        &self.lane,
                        "No active run to abort on this lane",
                    ));
                }
                None if idle_is_ok => None,
                None => {
                    return Err(HarnessError::no_active_run(
                        &self.lane,
                        "No active run to abort",
                    ));
                }
            }
        };
        let queued = self.clear_queue_locked().await?;
        Ok(AbortResult {
            run_id: run_id.unwrap_or_default(),
            steer: queued
                .steering
                .into_iter()
                .map(|m| m.into_message())
                .collect(),
            follow_up: queued
                .follow_up
                .into_iter()
                .map(|m| m.into_message())
                .collect(),
        })
    }

    /// "Finish the current turn, then stop" — the gentle counterpart of abort.
    ///
    /// Abort interrupts the in-flight provider call and running tools; this only
    /// prevents the loop from starting **another** turn, so the current answer
    /// is not truncated.
    ///
    /// One-shot: consumed at the next turn boundary, so it never leaks into a
    /// later prompt. Setting it while idle is a harmless no-op.
    pub fn request_stop_round(&self) {
        self.stop_round
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Construct from options. Existing sessions require explicit restore opt-in.
    pub async fn create(options: AgentHarnessOptions) -> HarnessResult<Self> {
        if !options.allow_existing_session {
            let existing = options
                .session
                .find_records(&RecordQuery {
                    limit: Some(1),
                    ..Default::default()
                })
                .await
                .map_err(session_to_harness_err)?;
            if !existing.is_empty() {
                return Err(HarnessError::io(
                    "create.restore is not implemented: session already has records",
                ));
            }
        }

        if options.allow_existing_session {
            options
                .session
                .prune_completed_progress()
                .await
                .map_err(session_to_harness_err)?;
        }

        // Recovery repaired whatever the previous process left behind. If that
        // repair left the branch mid-loop (a tool result as the tip), the run can
        // be continued instead of waiting for the user to restate the request.
        // Anything still queued when the previous process died is restored here.
        // For a fresh session this is empty, and for the non-restore path there is
        // nothing to read.
        let rebuilt_queue = if options.allow_existing_session {
            Self::rebuild_queues(&options.session, "main").await?
        } else {
            Vec::new()
        };

        let mut pending_resume = None;
        if options.allow_existing_session {
            let interrupted = options
                .session
                .find_open_operations("main", Some(1))
                .await
                .map_err(session_to_harness_err)?
                .first()
                .cloned();
            // A run that died waiting to retry is continued too, and its failed
            // attempt is not salvaged. Decided before recovery so the salvage and
            // the resume gate agree.
            let died_awaiting_retry = match &interrupted {
                Some(dead) => Self::died_awaiting_retry(&options.session, &dead.base.id).await,
                None => false,
            };
            Self::finish_interrupted_operation(
                &options.session,
                "main",
                if options.before_tool_call.is_none() {
                    &options.tools
                } else {
                    &[]
                },
                &CancellationToken::new(),
                !died_awaiting_retry,
                None,
            )
            .await?;
            if let Some(dead) = interrupted {
                let died_before_any_output =
                    Self::tip_is_user_message(&options.session, "main").await;
                if died_awaiting_retry
                    || died_before_any_output
                    || Self::lane_is_resumable(&options.session, "main").await
                {
                    let (chain_origin_run_id, attempt) = Self::resume_chain(&dead);
                    pending_resume = Some(PendingResume {
                        interrupted_run_id: dead.base.id.clone(),
                        chain_origin_run_id,
                        attempt,
                    });
                }
            }
        }

        let child_scope = Arc::new(Mutex::new(CancellationToken::new()));
        let inner = HarnessInner {
            child_scope: child_scope.clone(),
            execution_scope: None,
            skill_execution_env: None,
            model: options.model,
            thinking_level: options.thinking_level,
            active_tool_names: options.active_tool_names,
            exact_resources: false,
            tools: options.tools,
            resources: options.resources,
            stream_options: options.stream_options,
            retry: options.retry,
            compaction: options.compaction,
            driving: options.drive,
            tool_execution: options.tool_execution,
            system_prompt: options.system_prompt,
            models: options.models,
            to_provider_messages: options.to_provider_messages,
            entry_projectors: options.entry_projectors,
            entry_transforms: options.entry_transforms,
            agent_emitter: options.agent_emitter,
            pending_resume,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            transform_context: options.transform_context,
            provider_hooks: options.provider_hooks,
            closed: false,
            active_run: None,
            steering_queue: Arc::new(Mutex::new(MessageQueue::new(options.steering_mode))),
            follow_up_queue: Arc::new(Mutex::new(MessageQueue::new(options.follow_up_mode))),
            next_run_queue: Arc::new(Mutex::new(MessageQueue::new(QueueMode::All))),
            pending_context_lanes: Default::default(),
        };
        for (kind, item) in rebuilt_queue {
            let queue = match kind {
                QueueKind::Steer => &inner.steering_queue,
                QueueKind::FollowUp => &inner.follow_up_queue,
                QueueKind::NextRun => &inner.next_run_queue,
                // Context writes are drained from durable records at boundaries.
                QueueKind::Context => continue,
            };
            queue.lock().unwrap().pending.push_back(item);
        }

        let harness = Self {
            session: options.session,
            inner: Arc::new(Mutex::new(inner)),
            bus: HarnessEventBus::new(),
            lane: "main".to_string(),
            message_writes: Arc::new(tokio::sync::Mutex::new(())),
            write_failures: Arc::new(Mutex::new(Vec::new())),
            unsaved_messages: Arc::new(Mutex::new(Vec::new())),
            child_scope,
            is_child: false,
            max_turns: None,
            stop_round: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        if options.allow_existing_session {
            let messages = harness.flush_context_messages_locked().await?;
            harness.emit_appended_messages(messages).await;
        }
        Ok(harness)
    }

    /// Whether the run died waiting to retry a retryable failure.
    ///
    /// Reads the durable `retry_pending` record: the frames cannot answer this
    /// (a failed attempt leaves no terminal frame, so its reduced message looks
    /// merely `pending`), and a failed attempt leaves no other trace because a
    /// `step_attempt` is only written when a message commits.
    ///
    /// Only the latest retry intent matters. A frame binding naming its reserved
    /// result means generation has started, so those new frames must be salvaged
    /// normally rather than discarded as the previous failed attempt.
    async fn died_awaiting_retry(session: &Session, run_id: &str) -> bool {
        let records = match session
            .find_records(&RecordQuery {
                record_type: Some("retry_pending"),
                run_id: Some(run_id.to_string()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
        {
            Ok(records) => records,
            Err(_) => return false,
        };
        let Some(pending) = records.into_iter().rev().find_map(|record| match record {
            LaneRecord::RetryPending(pending) => Some(pending),
            _ => None,
        }) else {
            return false;
        };
        if session
            .get_entry(&pending.result_entry_id)
            .await
            .ok()
            .flatten()
            .is_some()
        {
            return false;
        }
        let frames = match session
            .find_records(&RecordQuery {
                record_type: Some(crate::frame_progress::ASSISTANT_FRAME_RECORD_TYPE),
                run_id: Some(run_id.to_string()),
                ..Default::default()
            })
            .await
        {
            Ok(frames) => frames,
            Err(_) => return false,
        };
        !frames.into_iter().any(|record| matches!(record,
            LaneRecord::AssistantFrame(frame)
                if frame.base.seq > pending.base.seq
                    && frame.op == crate::session::types::AssistantFrameOp::CommitStream
                    && frame.frame.as_ref().and_then(|value| value.get("entryId")).and_then(|value| value.as_str()) == Some(pending.result_entry_id.as_str())
        ))
    }

    /// Whether the lane's branch ends in a state a run can continue from.
    ///
    /// True when the last entry is a tool result: a run that reaches its tool
    /// results always goes on to ask the provider for the next assistant message,
    /// so a tool result as the tip means the run died before it could. A tool that
    /// set `terminate` legitimately ends the run there, so it is excluded.
    async fn lane_is_resumable(session: &Session, lane: &str) -> bool {
        // Preserve legacy recovery's best-effort read behavior. Explicit
        // transitions instead use the fallible path and fail closed.
        Self::lane_is_resumable_impl(session, lane, false, None)
            .await
            .unwrap_or(false)
    }

    async fn lane_is_resumable_checked(session: &Session, lane: &str) -> HarnessResult<bool> {
        Self::lane_is_resumable_impl(session, lane, true, None).await
    }

    async fn lane_is_resumable_impl(
        session: &Session,
        lane: &str,
        strict: bool,
        deadline: Option<std::time::Instant>,
    ) -> HarnessResult<bool> {
        // Pin one leaf: a concurrent append/move must not change the branch
        // between batches. Bound payload hydration even for very long histories.
        let view = session.view(lane);
        let leaf = match view.get_leaf_id().await {
            Ok(Some(leaf)) => leaf,
            Ok(None) => return Ok(false),
            Err(error) if strict => return Err(session_to_harness_err(error)),
            Err(_) => return Ok(false),
        };
        let bounds = BranchBounds {
            start: Some(leaf),
            ..Default::default()
        };
        let mut cursor = None;
        loop {
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return Err(HarnessError::io("saved read processing deadline exceeded"));
            }
            let entries = match view
                .find_entries_on_branch(
                    &EntryQuery {
                        exclude_tool_program_bodies: true,
                        order: Some(EntryOrder::NewestFirst),
                        limit: Some(16),
                        cursor,
                        ..Default::default()
                    },
                    &bounds,
                )
                .await
            {
                Ok(entries) => entries,
                Err(error) if strict => return Err(session_to_harness_err(error)),
                Err(_) => return Ok(false),
            };
            let Some(last) = entries.last() else {
                return Ok(false);
            };
            cursor = Some(EntryCursor {
                after_seq: last.base().seq,
            });
            for entry in &entries {
                let message = match entry {
                    Entry::Message(message) => message,
                    Entry::Custom(_) => continue, // extension data is not model input
                    _ => return Ok(false),
                };
                let queue = Self::queued_entry_kind_checked(session, lane, &message.base.id).await;
                let queue = if strict {
                    queue?
                } else {
                    queue.unwrap_or(None)
                };
                match queue {
                    Some(QueueKind::Steer | QueueKind::FollowUp) => return Ok(true),
                    // A silent context update doesn't request a reply. Keep
                    // looking, including across a batch boundary.
                    Some(QueueKind::Context) => continue,
                    _ => {
                        return Ok(matches!(message.message, AgentMessage::ToolResult(_))
                            && message.terminate != Some(true))
                    }
                }
            }
        }
    }

    /// Recovery uses the accepted delivery mode, not a custom message's role,
    /// to distinguish queued continuations from context-only updates.
    async fn queued_entry_kind_checked(
        session: &Session,
        lane: &str,
        entry_id: &str,
    ) -> HarnessResult<Option<QueueKind>> {
        // The old newest-first scan collected into a map, so duplicate target
        // ids were overwritten by the OLDEST enqueue. Preserve that behavior,
        // filtering before copying payloads and returning at most one record.
        Ok(session
            .find_records(&RecordQuery {
                lane: Some(lane.to_string()),
                record_type: Some("queue_enqueued"),
                target_entry_id: Some(entry_id.to_string()),
                order: Some(EntryOrder::OldestFirst),
                limit: Some(1),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)?
            .into_iter()
            .find_map(|record| match record {
                LaneRecord::QueueEnqueued(record) => Some(record.queue),
                _ => None,
            }))
    }

    /// The resume chain a run belongs to: `(origin run id, resumes so far)`.
    ///
    /// Read from the run's own durable intent. The count has to be durable,
    /// because the crash that makes it matter also destroys any in-memory counter
    /// that could have held it — and a run that crashes deterministically would
    /// otherwise be re-driven on every startup, spending tokens and repeating side
    /// effects while the user only ever sees a crash.
    fn resume_chain(started: &OperationStartedRecord) -> (String, u32) {
        match &started.intent {
            OperationIntent::Run {
                resume_data: Some(data),
                ..
            } => {
                let origin = data
                    .get("resumedFrom")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| started.base.id.clone());
                let prior = data
                    .get("resumeAttempt")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as u32;
                (origin, prior)
            }
            // A run with no resume data is the chain's origin.
            _ => (started.base.id.clone(), 0),
        }
    }

    /// The compaction budget in force, so a client can show how close the
    /// context is to the threshold without duplicating the defaults.
    pub fn compaction_settings(&self) -> crate::types::CompactionSettings {
        self.inner.lock().unwrap().compaction.clone()
    }

    /// The run recovery repaired and that is waiting to be continued, if any.
    pub fn pending_resume(&self) -> Option<PendingResume> {
        self.inner
            .lock()
            .unwrap()
            .pending_resume
            .clone()
            .filter(|pending| pending.attempt < MAX_RESUME_ATTEMPTS)
    }

    /// Continue the interrupted run, if recovery left one to continue.
    ///
    /// Returns `Ok(None)` when there is nothing to resume, when the resume budget
    /// is exhausted, or when the branch is not in a continuable state — in all of
    /// those cases the caller should simply carry on with a normal prompt.
    ///
    /// This is a *product-visible* action: it spends tokens and runs tools without
    /// the user asking a question, which is why it is bounded and why it announces
    /// itself in the transcript.
    pub async fn resume_pending(&self) -> HarnessResult<Option<RunResult>> {
        // Take admission before any session reads or notices. Clearing the
        // pending marker on a session swap cannot cancel a resume that already
        // took it; the operation lease must protect that whole preparation.
        let gate = self.message_writes.lock().await;
        {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            if inner.pending_resume.is_none() {
                return Ok(None);
            }
        }
        let admission = self.acquire_run_locked(OperationKind::Run)?;
        let _run_lease = ActiveRunLease {
            harness: self,
            run_id: admission.0.clone(),
        };
        // A rejected admission leaves the marker intact for a later attempt.
        let pending = self.inner.lock().unwrap().pending_resume.take().unwrap();
        drop(gate);
        if pending.attempt >= MAX_RESUME_ATTEMPTS {
            tracing::warn!(
                run_id = %pending.interrupted_run_id,
                attempts = pending.attempt,
                "not resuming the interrupted run again: its resume budget is exhausted"
            );
            self.record_resume_notice(&format!(
                "The interrupted run was not resumed again: it has already been resumed {} times, \
                 and resuming it again could repeat its side effects without making progress.",
                pending.attempt
            ))
            .await;
            return Ok(None);
        }
        // The one hard requirement is that the continuation is a *valid request*:
        // a trailing assistant message is rejected by providers (and by
        // `run_agent_loop_continue`), so that case must not be resumed even if a
        // pending resume was recorded. Whether a resume is *offered* in the first
        // place is decided in `create`; this is the last-line check.
        if Self::tip_is_assistant(&self.session, &self.lane).await {
            tracing::warn!(
                lane = %self.lane,
                "not continuing the interrupted run: the branch ends at an assistant message"
            );
            return Ok(None);
        }
        self.record_resume_notice(&format!(
            "Continuing the run that was interrupted (resume {} of {}).",
            pending.attempt + 1,
            MAX_RESUME_ATTEMPTS
        ))
        .await;
        // An empty prompt is exactly a continuation: the loop's next action is the
        // provider call the interrupted run never reached.
        self.run_core_with_admission(Vec::new(), false, admission, Some(pending))
            .await
            .map(Some)
    }

    /// Record the user-visible "why is a run starting by itself" notice.
    async fn record_resume_notice(&self, text: &str) {
        let notice = crate::messages::create_custom_message(
            "runResume",
            UserContent::Text(text.to_string()),
            true,
            None,
            now_ms(),
        );
        if let Err(error) = self.session.view(&self.lane).append_message(notice).await {
            tracing::warn!(
                lane = %self.lane,
                %error,
                "could not record the run-resume notice"
            );
        }
    }
    /// The event bus. Callers register `RunStart`/`RunEnd` listeners here.
    pub fn events(&self) -> &HarnessEventBus {
        &self.bus
    }

    /// The durable id of the session this harness is currently bound to.
    /// Returns an owned `String` (the metadata read hands back a temporary),
    /// and stays synchronous so write failure paths can stamp records without
    /// introducing an await.
    pub fn session_id(&self) -> String {
        self.session.storage().metadata().id
    }

    /// The session facade (for direct entry/record queries the harness API
    /// doesn't surface).
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Wait for every in-flight durable write to settle.
    ///
    /// A mutation is acknowledged to its caller only after the backend commits
    /// it, but a turn's writes are issued by the harness while the UI is already
    /// painting the result. An exit that aborts the tasks carrying those writes
    /// can therefore leave a turn the user saw with no durable counterpart.
    /// Callers on a shutdown path must await this before aborting them.
    pub async fn drain_session(&self) {
        self.session.drain().await;
    }

    /// Record a session write that failed while the run kept going.
    ///
    /// Keeps the `tracing::warn!` at the call site: the log is still the place a
    /// developer looks. The queue exists because a warning inside an alternate
    /// screen reaches nobody, and "your conversation stopped being saved" is the
    /// one failure a user must see before exiting.
    ///
    /// Dropping past [`MAX_WRITE_FAILURES`] is deliberate: a backend failing every
    /// write would otherwise grow this for as long as the run lasts, and the host
    /// drains at every turn boundary anyway.
    fn record_write_failure(&self, what: &str, error: &SessionError) {
        record_write_failure(&self.write_failures, what, &error.message);
    }

    /// Record a failure whose error type is already flattened to a message.
    fn record_write_failure_message(&self, what: &str, message: &str) {
        record_write_failure(&self.write_failures, what, message);
    }

    /// Keep a message whose append was refused so the export path can still
    /// write it.
    ///
    /// The recovery guidance tells the user to export before exiting, but
    /// `export_session` reads session entries and a refused append never becomes
    /// one. Retaining the message here is what makes that guidance true rather
    /// than merely hopeful. Bounded like [`MAX_WRITE_FAILURES`]: a backend that
    /// refuses every write during a long run must not grow this without limit.
    ///
    /// The copy is stamped with the session and lane the message was written
    /// on. The vector is harness-wide and outlives `set_session` switches, so
    /// the stamp is what lets `unsaved_messages` hand each session back only its
    /// own recovery data — discarding it instead would destroy the recovery the
    /// user was promised when they return to the original session.
    fn keep_unsaved_message(&self, what: &str, message: AgentMessage) {
        // Resolve the stamp before taking the vector lock: both reads go through
        // the shared binding with no await in between, so the stamp always
        // describes the session the caller was writing to.
        let (session_id, lane) = (self.session_id(), self.lane.clone());
        let mut unsaved = self
            .unsaved_messages
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if unsaved.len() >= MAX_WRITE_FAILURES {
            return;
        }
        unsaved.push(UnsavedMessage {
            message,
            what: what.to_string(),
            at: now_ms(),
            session_id,
            lane,
        });
    }

    /// Test-only entry point for [`Self::keep_unsaved_message`].
    ///
    /// The production path is `append_message` failing, which needs a hostile
    /// backend; callers that want to check what the *host* does with a recovery
    /// copy (the export) should not have to rebuild that backend.
    #[doc(hidden)]
    pub fn keep_unsaved_message_for_test(&self, what: &str, message: AgentMessage) {
        self.keep_unsaved_message(what, message);
    }

    /// Every message whose append was refused and that storage therefore never
    /// applied, oldest first, scoped to the session and lane this harness is
    /// currently bound to.
    ///
    /// Read-only on purpose: the host consults it while building an export,
    /// which must stay repeatable (a user who exports twice after one failure
    /// gets the same file, and a second export after another failure includes
    /// both). The copies live as long as the harness, which is the same window
    /// in which the transcript is still on screen — and they are retained, not
    /// cleared, across `set_session` switches, so returning to the original
    /// session restores access to its recovery data while another session's
    /// export can never pick them up.
    pub fn unsaved_messages(&self) -> Vec<UnsavedMessage> {
        let session_id = self.session_id();
        self.unsaved_messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|copy| copy.session_id == session_id && copy.lane == self.lane)
            .cloned()
            .collect()
    }

    /// Take the write failures recorded since the last drain.
    ///
    /// The host calls this at `AgentEnd` and when a run finishes, then renders
    /// them: a failed write is not a run failure, so it cannot ride the run
    /// outcome. Lane handles share the queue with the harness that spawned them.
    pub fn take_write_failures(&self) -> Vec<HarnessWriteFailure> {
        let mut failures = self
            .write_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *failures)
    }

    /// Return a runner bound to a session lane. All lanes share configuration,
    /// cancellation, and the event bus, while persistence and context use the
    /// selected lane's branch. The lane must already exist in the session.
    pub fn lane(&self, name: &str) -> Arc<dyn AgentLane> {
        Arc::new(Self {
            session: self.session.clone(),
            inner: Arc::clone(&self.inner),
            bus: self.bus.clone(),
            lane: name.to_string(),
            message_writes: Arc::clone(&self.message_writes),
            write_failures: Arc::clone(&self.write_failures),
            unsaved_messages: Arc::clone(&self.unsaved_messages),
            child_scope: self.child_scope.clone(),
            is_child: self.is_child,
            max_turns: self.max_turns,
            stop_round: Arc::clone(&self.stop_round),
        })
    }

    /// The harness's bound lane name ("main" for a top-level harness).
    pub fn lane_name(&self) -> &str {
        &self.lane
    }

    // -- durable-session read/write surface (TS `AgentHarness` getTipId /
    //    findEntries / appendMessage / …). Each delegates to the lane-scoped
    //    `SessionTree` view so non-main lanes behave identically. -----------

    /// The lane-scoped session view backing the read/write surface below.
    pub fn view(&self) -> Arc<dyn SessionTree> {
        self.session.view(&self.lane)
    }

    /// `getTipId()` — the lane's current leaf entry id. Mirrors TS
    /// `AgentHarness.getTipId`.
    pub async fn get_tip_id(&self) -> HarnessResult<Option<String>> {
        self.view()
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    /// `findEntries(query)` — query the lane's entries. Mirrors TS
    /// `AgentHarness.findEntries`.
    pub async fn find_entries(&self, query: &EntryQuery) -> HarnessResult<Vec<Entry>> {
        self.view()
            .find_entries(query)
            .await
            .map_err(session_to_harness_err)
    }

    /// `findEntry(query)` — first matching entry on the lane. Mirrors TS
    /// `AgentHarness.findEntry`.
    pub async fn find_entry(&self, query: &EntryQuery) -> HarnessResult<Option<Entry>> {
        self.view()
            .find_entry(query)
            .await
            .map_err(session_to_harness_err)
    }

    /// `findEntriesOnBranch(query, bounds)` — branch path of the lane.
    pub async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> HarnessResult<Vec<Entry>> {
        self.view()
            .find_entries_on_branch(query, bounds)
            .await
            .map_err(session_to_harness_err)
    }

    /// Optional storage-selected transcript page. `None` asks the caller to
    /// use its legacy projection; storage errors must not trigger that fallback.
    pub async fn transcript_page(
        &self,
        query: &crate::session::types::TranscriptPageQuery,
    ) -> HarnessResult<Option<crate::session::types::TranscriptPage>> {
        self.view()
            .transcript_page(query)
            .await
            .map_err(session_to_harness_err)
    }

    /// Optional bounded, hook-free context token estimate. Unsupported storage
    /// returns `None`; actual errors must not trigger a full-body fallback.
    pub async fn context_usage(
        &self,
    ) -> HarnessResult<Option<crate::compaction::ContextUsageEstimate>> {
        self.view()
            .context_usage()
            .await
            .map_err(session_to_harness_err)
    }

    /// Bounded user-only summaries on this harness's selected lane.
    pub async fn message_summaries(
        &self,
        query: &crate::session::types::MessageSummaryQuery,
    ) -> HarnessResult<crate::session::types::MessageSummaryPage> {
        self.view()
            .message_summaries(query)
            .await
            .map_err(session_to_harness_err)
    }
    /// Bounded, bidirectionally pageable transcript context on the selected lane.
    pub async fn message_window(
        &self,
        query: &crate::session::types::MessageWindowQuery,
    ) -> HarnessResult<crate::session::types::MessageWindowPage> {
        self.view()
            .message_window(query)
            .await
            .map_err(session_to_harness_err)
    }

    /// `getEntry(id)` — look up a single entry by id. Mirrors TS
    /// `AgentHarness.getEntry`.
    pub async fn get_entry(&self, id: &str) -> HarnessResult<Option<Entry>> {
        self.view()
            .get_entry(id)
            .await
            .map_err(session_to_harness_err)
    }

    /// `appendMessage(message)` — persist an out-of-band message entry on the
    /// lane without running the agent. During an operation the message is
    /// durably queued until a turn boundary. The returned id is reserved, so
    /// `get_entry` may return `None` until the message has been committed.
    pub async fn append_message(&self, message: AgentMessage) -> HarnessResult<String> {
        let gate = self.message_writes.lock().await;
        let result = self.append_message_locked(message.clone()).await;
        drop(gate);
        let (id, _, appended) = match result {
            Ok(appended) => appended,
            Err(error) => {
                // A user message that failed to persist is the clearest form of
                // "your conversation is not being saved": the text is on screen
                // and the write was refused. Record it for the host and keep the
                // error flowing to the caller.
                self.record_write_failure_message("a message you sent", &error.to_string());
                // Keep a copy the export path can read: JSONL storage applies an
                // entry only after the append succeeds, so a refused message has
                // no representation in the session view `/export` reads.
                self.keep_unsaved_message("a message you sent", message);
                return Err(error);
            }
        };
        self.emit_appended_messages(appended).await;
        Ok(id)
    }

    /// Caller holds the message admission gate.
    async fn append_message_locked(
        &self,
        message: AgentMessage,
    ) -> HarnessResult<(String, bool, Vec<(String, AgentMessage)>)> {
        let active = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner
                .active_run
                .as_ref()
                .is_some_and(|run| run.lane == self.lane)
        };
        let operation_open = !self
            .session
            .find_open_operations(&self.lane, Some(1))
            .await
            .map_err(session_to_harness_err)?
            .is_empty();
        if active || operation_open {
            let entry_id = self.next_entry_id();
            // Not tied to the operation: accepted context must survive aborts,
            // retries and the operation finishing before its final drain.
            self.write_queue_enqueued(QueueKind::Context, &entry_id, &message)
                .await?;
            self.inner
                .lock()
                .unwrap()
                .pending_context_lanes
                .insert(self.lane.clone());
            return Ok((entry_id, true, Vec::new()));
        }
        let mut appended = self.flush_context_messages_locked().await?;
        let id = self
            .view()
            .append_message(message.clone())
            .await
            .map_err(session_to_harness_err)?;
        appended.push((id.clone(), message));
        Ok((id, false, appended))
    }

    /// Send a message through the shared lane delivery queues. Like Pi, default
    /// idle sends are context-only, default active sends steer, and `nextTurn`
    /// never starts or joins the current run. Admission and delivery selection
    /// are serialized so a concurrent prompt cannot turn an idle send into a
    /// reentrant run or insert it inside a tool batch.
    pub async fn send_message(
        &self,
        message: AgentMessage,
        options: SendMessageOptions,
    ) -> HarnessResult<SendMessageResult> {
        let message = self.expand_skill_input(message, None).await;
        let message = normalize_user_images(message).await?;
        let gate = self.message_writes.clone().lock_owned().await;
        let active = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner
                .active_run
                .as_ref()
                .is_some_and(|run| run.lane == self.lane)
        };
        let queue = match options.deliver_as {
            MessageDelivery::NextTurn => Some(QueueKind::NextRun),
            MessageDelivery::FollowUp if active && options.trigger_turn != Some(false) => {
                Some(QueueKind::FollowUp)
            }
            MessageDelivery::Steer if active && options.trigger_turn != Some(false) => {
                Some(QueueKind::Steer)
            }
            _ => None,
        };
        if let Some(queue) = queue {
            // Sends can arrive before operation_started or after abort/finish.
            // Their acceptance belongs to the lane, not the current operation.
            let queued = self.enqueue_message_locked(message, queue, gate).await?;
            return Ok(SendMessageResult::Queued {
                entry_id: queued.entry_id,
                queue,
            });
        }
        if !active && options.trigger_turn == Some(true) {
            let admission = self.acquire_run_locked(OperationKind::Run)?;
            drop(gate);
            return self
                .run_core_with_admission(vec![message], false, admission, None)
                .await
                .map(SendMessageResult::Run);
        }
        let (entry_id, queued, messages) = self.append_message_locked(message).await?;
        drop(gate);
        self.emit_appended_messages(messages).await;
        Ok(if queued {
            SendMessageResult::Queued {
                entry_id,
                queue: QueueKind::Context,
            }
        } else {
            SendMessageResult::Appended { entry_id }
        })
    }

    /// Pending context targets are consumed by committing their reserved entry,
    /// not by cancelling the enqueue first. A crash between those two writes
    /// would otherwise lose an acknowledged message. Entry existence makes
    /// replay idempotent, including a crash immediately after the commit.
    async fn pending_context_entries(&self) -> HarnessResult<Vec<ProvisionedEntry>> {
        let records = self
            .session
            .find_records(&RecordQuery {
                lane: Some(self.lane.clone()),
                record_type: Some("queue_enqueued"),
                queue_kind: Some(QueueKind::Context),
                pending_queue_only: true,
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)?;
        let mut pending = Vec::new();
        for record in records {
            let LaneRecord::QueueEnqueued(record) = record else {
                continue;
            };
            if record.queue != QueueKind::Context {
                continue;
            }
            let id = Self::target_entry_id(&record.target)
                .ok_or_else(|| HarnessError::io("context enqueue has no entry id"))?;
            if self
                .view()
                .get_entry(id)
                .await
                .map_err(session_to_harness_err)?
                .is_some()
            {
                continue;
            }
            let message = Self::message_from_target(&record.target)
                .ok_or_else(|| HarnessError::io("context enqueue has an invalid message"))?;
            if !self.notice_source_on_branch(&message).await? {
                continue;
            }
            pending.push(ProvisionedEntry {
                id: id.to_string(),
                kind: ProvisionedKind::Message {
                    message,
                    terminate: None,
                },
            });
        }
        Ok(pending)
    }

    /// Caller owns `message_writes`, or is constructing an unshared harness.
    async fn commit_context_entries(
        &self,
        entries: Vec<ProvisionedEntry>,
    ) -> HarnessResult<Vec<(String, AgentMessage)>> {
        let mut messages = Vec::with_capacity(entries.len());
        for entry in entries {
            let ProvisionedKind::Message { message, .. } = &entry.kind else {
                unreachable!()
            };
            let message = message.clone();
            let id = entry.id.clone();
            self.append_provisioned_locked(entry).await?;
            messages.push((id, message));
        }
        Ok(messages)
    }

    async fn flush_context_messages_locked(&self) -> HarnessResult<Vec<(String, AgentMessage)>> {
        let messages = self
            .commit_context_entries(self.pending_context_entries().await?)
            .await?;
        self.inner
            .lock()
            .unwrap()
            .pending_context_lanes
            .remove(&self.lane);
        Ok(messages)
    }

    fn has_pending_context(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .pending_context_lanes
            .contains(&self.lane)
    }

    /// Context-only messages are already durable. Emit directly to the client
    /// emitter so they do not increment the loop's persistence indices.
    async fn emit_appended_messages(&self, messages: Vec<(String, AgentMessage)>) {
        let emitter = self.inner.lock().unwrap().agent_emitter.clone();
        if let Some(emitter) = emitter {
            for (id, message) in messages {
                emitter
                    .emit(AgentEvent::MessageStart {
                        message_id: Some(id.clone()),
                        message: message.clone(),
                    })
                    .await;
                emitter
                    .emit(AgentEvent::MessageEnd {
                        message,
                        message_id: Some(id),
                    })
                    .await;
            }
        }
    }

    /// Commit the current turn before appending external context, then return
    /// that context to the running loop. This also handles a final assistant
    /// message that normally would only be persisted at run end.
    async fn append_context_at_boundary(
        &self,
        assistant: &AssistantMessage,
        new_messages: &[AgentMessage],
        settle: &crate::settle::SettleState,
    ) -> HarnessResult<Vec<AgentMessage>> {
        let retry = self.inner.lock().unwrap().retry.clone();
        if crate::settle::will_retry(
            assistant,
            retry.enabled,
            settle.attempt(),
            retry.max_retries,
        ) {
            return Ok(Vec::new());
        }
        let gate = self.message_writes.lock().await;
        let pending = self.pending_context_entries().await?;
        if pending.is_empty() {
            return Ok(Vec::new());
        }
        let committed = settle.committed_indices();
        for (index, message) in new_messages.iter().enumerate() {
            if committed.contains(&index) {
                continue;
            }
            let entry_id = settle.reserve_entry_id(index, self);
            settle.prepare_commit(index, &entry_id, self).await?;
            self.append_provisioned_locked(ProvisionedEntry {
                id: entry_id.clone(),
                kind: ProvisionedKind::Message {
                    message: message.clone(),
                    terminate: None,
                },
            })
            .await?;
            settle.record_commit(index, &entry_id);
        }
        let messages = self.commit_context_entries(pending).await?;
        self.inner
            .lock()
            .unwrap()
            .pending_context_lanes
            .remove(&self.lane);
        drop(gate);
        self.emit_appended_messages(messages.clone()).await;
        Ok(messages.into_iter().map(|(_, message)| message).collect())
    }

    /// Finish callbacks before admitting another operation. RunEnd and the
    /// returned outcome use the same snapshot; writes from RunEnd listeners
    /// are drained afterward without changing that completed run's leaf.
    async fn finish_context_writes(
        &self,
        run_id: &str,
        end: Option<RunEndOutcome>,
    ) -> HarnessResult<Option<String>> {
        self.finish_context_writes_with_result(run_id, end, None)
            .await
    }

    async fn finish_context_writes_with_result(
        &self,
        run_id: &str,
        end: Option<RunEndOutcome>,
        mut terminal: Option<(
            OperationOutcome,
            Option<OperationError>,
            crate::session::types::RunResultIdentity,
        )>,
    ) -> HarnessResult<Option<String>> {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(active) = inner
                .active_run
                .as_mut()
                .filter(|active| active.run_id == run_id)
            {
                active.finishing = true;
            }
        }
        let mut first_drain = true;
        let mut end_emitted = false;
        let mut terminal_leaf = None;
        loop {
            let gate = self.message_writes.lock().await;
            let messages = if first_drain || self.has_pending_context() {
                self.flush_context_messages_locked().await?
            } else {
                Vec::new()
            };
            first_drain = false;
            if !messages.is_empty() {
                drop(gate);
                self.emit_appended_messages(messages).await;
                continue;
            }
            if !end_emitted {
                terminal_leaf = self
                    .view()
                    .get_leaf_id()
                    .await
                    .map_err(session_to_harness_err)?;
                if let Some((outcome, error, mut identity)) = terminal.take() {
                    identity.leaf_id = terminal_leaf.clone().unwrap_or_default();
                    self.session
                        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                            base: self.record_base(),
                            run_id: run_id.to_owned(),
                            outcome,
                            error,
                            result: Some(identity),
                        }))
                        .await
                        .map_err(session_to_harness_err)?;
                }
                if let Some(outcome) = end {
                    drop(gate);
                    // External listeners run with no locks held, while the
                    // operation's lease still prevents overlapping admission.
                    self.bus.emit(&HarnessEvent::RunEnd(RunEndEvent {
                        lane: self.lane.clone(),
                        run_id: run_id.to_string(),
                        outcome,
                        leaf_id: terminal_leaf.clone().unwrap_or_default(),
                    }));
                    end_emitted = true;
                    continue;
                }
            }
            self.release_run(run_id);
            drop(gate);
            return Ok(terminal_leaf);
        }
    }

    /// Compose ordered transcript persistence, boundary delivery and compaction.
    /// The common fully committed path borrows the context unchanged; only a
    /// pending write or an actual queued append needs a snapshot.
    fn context_boundary_hook(
        &self,
        next: Option<AfterToolResults>,
        settle: Arc<crate::settle::SettleState>,
        signal: CancellationToken,
        error: Arc<Mutex<Option<String>>>,
    ) -> AfterToolResults {
        let harness = self.clone();
        Arc::new(move |ctx: ShouldStopAfterTurnContext<'_>| {
            // Both after_tool_results and prepare_next_turn use this hook. Once
            // a boundary write failed, do not retry it again in the same turn
            // or invoke the next (possibly compaction/provider) callback.
            if error.lock().unwrap().is_some() {
                return Box::pin(async { None });
            }
            let prefix_len = settle.required_prefix_len(ctx.new_messages);
            let pending_prefix = settle.committed_count() < prefix_len;
            if !pending_prefix && !harness.has_pending_context() {
                return match &next {
                    Some(next) => next(ctx),
                    None => Box::pin(async { None }),
                };
            }
            let harness = harness.clone();
            let settle = Arc::clone(&settle);
            let signal = signal.clone();
            let error = Arc::clone(&error);
            let next = next.clone();
            let assistant = ctx.message.clone();
            let tool_results = ctx.tool_results.to_vec();
            let new_messages = ctx.new_messages.to_vec();
            let mut context = ctx.context.clone();
            Box::pin(async move {
                if pending_prefix {
                    if let Err(failure) = harness
                        .persist_new_messages(&new_messages[..prefix_len], 0, &settle)
                        .await
                    {
                        *error.lock().unwrap() = Some(failure.to_string());
                        signal.cancel();
                        return None;
                    }
                }
                let messages = match harness
                    .append_context_at_boundary(&assistant, &new_messages, &settle)
                    .await
                {
                    Ok(messages) => messages,
                    Err(failure) => {
                        *error.lock().unwrap() = Some(failure.to_string());
                        signal.cancel();
                        return None;
                    }
                };
                let changed = !messages.is_empty();
                context.messages.extend(messages);
                if let Some(next) = next {
                    if let Some(update) = next(ShouldStopAfterTurnContext {
                        message: &assistant,
                        tool_results: &tool_results,
                        new_messages: &new_messages,
                        context: &context,
                    })
                    .await
                    {
                        return Some(update);
                    }
                }
                changed.then_some(rpi_agent::AgentLoopTurnUpdate {
                    context: Some(context),
                    ..Default::default()
                })
            })
        })
    }

    /// `appendCustomEntry(customType, data?)` — persist a custom entry on the
    /// lane. Mirrors TS `AgentHarness.appendCustomEntry`.
    pub async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> HarnessResult<String> {
        let _gate = self.message_writes.lock().await;
        self.view()
            .append_custom_entry(custom_type, data)
            .await
            .map_err(session_to_harness_err)
    }

    /// `getName()` — the session name. Mirrors TS `AgentHarness.getName`.
    pub async fn get_name(&self) -> HarnessResult<Option<String>> {
        self.view().get_name().await.map_err(session_to_harness_err)
    }

    /// `setName(name)` — set/clear the session name. Mirrors TS
    /// `AgentHarness.setName`.
    pub async fn set_name(&self, name: Option<&str>) -> HarnessResult<()> {
        self.view()
            .set_name(name)
            .await
            .map_err(session_to_harness_err)
    }

    /// `getLabel(targetId)` — the label attached to an entry, when set.
    pub async fn get_label(&self, target_id: &str) -> HarnessResult<Option<String>> {
        self.view()
            .get_label(target_id)
            .await
            .map_err(session_to_harness_err)
    }

    /// `setLabel(targetId, label)` — attach/clear an entry label.
    pub async fn set_label(&self, target_id: &str, label: Option<&str>) -> HarnessResult<()> {
        self.view()
            .set_label(target_id, label)
            .await
            .map_err(session_to_harness_err)
    }

    /// `getStats()` — rolled-up token/cost/message counters.
    pub async fn get_stats(&self) -> HarnessResult<SessionStats> {
        self.view()
            .get_stats()
            .await
            .map_err(session_to_harness_err)
    }

    /// `snapshot()` — the lane read model (leaf, name, stats, active op).
    /// Mirrors the TS runtime lane snapshot. `labels` folds in every labelled
    /// entry the lane can see.
    pub async fn snapshot(&self) -> HarnessResult<LaneSnapshot> {
        let view = self.view();
        let leaf_id = view.get_leaf_id().await.map_err(session_to_harness_err)?;
        let stats = view.get_stats().await.ok();
        let name = view.get_name().await.ok().flatten();
        let active = self.active_operation_snapshot();
        let labels = self.lane_labels(&view).await;
        Ok(LaneSnapshot {
            lane: self.lane.clone(),
            leaf_id,
            name,
            stats,
            active,
            labels,
        })
    }

    /// `watchSession()` — watch this lane's event stream. Mirrors TS
    /// `AgentHarness.watchSession`: the listener receives every event on the
    /// harness bus. The returned [`HarnessWatcher`] unsubscribes on drop.
    pub fn watch_session<F>(&self, callback: F) -> HarnessWatcher
    where
        F: Fn(&HarnessEvent) + Send + Sync + 'static,
    {
        let mut handle = self.bus.watch(|| ());
        handle.start(watch_listener(callback));
        HarnessWatcher::new(handle)
    }

    /// `watch()` — harness-level alias of [`Self::watch_session`]. Mirrors the
    /// TS `AgentHarness.watch` entry point.
    pub fn watch<F>(&self, callback: F) -> HarnessWatcher
    where
        F: Fn(&HarnessEvent) + Send + Sync + 'static,
    {
        self.watch_session(callback)
    }

    /// `inspectExecution(toolCallId)` — locate the persisted tool-call entry
    /// (and its matching result, when present) for a tool call id. Mirrors the
    /// TS runtime `inspectExecution` read model.
    pub async fn inspect_execution(
        &self,
        tool_call_id: &str,
    ) -> HarnessResult<Option<ToolExecutionInfo>> {
        self.read_execution(tool_call_id).await
    }

    /// `getResult(runId)` — the terminal result of a completed run, read back
    /// from the durable `operation_finished` record. Mirrors TS
    /// `AgentHarness.getResult`.
    pub async fn get_result(&self, run_id: &str) -> HarnessResult<Option<RunResult>> {
        self.read_run_result(run_id).await
    }

    /// `accept(entryId)` — commit a pending/steering entry as a durable lane
    /// message. Returns `true` when the entry existed and was accepted.
    /// Mirrors the TS runtime accept step.
    pub async fn accept(&self, entry_id: &str) -> HarnessResult<bool> {
        let Some(entry) = self.get_entry(entry_id).await? else {
            return Ok(false);
        };
        let Entry::Message(me) = entry else {
            return Ok(false);
        };
        self.append_message(me.message).await?;
        Ok(true)
    }

    /// `requestAbort(runId)` — abort the named run when it is the active one.
    /// Returns `true` when the abort was delivered. Mirrors the TS runtime
    /// `requestAbort`.
    pub async fn request_abort(&self, run_id: &str) -> HarnessResult<bool> {
        let matches_active = {
            let inner = self.inner.lock().unwrap();
            inner
                .active_run
                .as_ref()
                .map(|a| a.run_id == run_id)
                .unwrap_or(false)
        };
        if !matches_active {
            return Ok(false);
        }
        AgentLane::abort(self).await?;
        Ok(true)
    }

    /// `drive(operationId)` — drive a parked deferred operation to its next
    /// disposition. When no suspended operation matches, reports
    /// [`DriveOutcome::Idle`]. Mirrors the TS runtime `drive` step.
    pub async fn drive(&self, operation_id: &str) -> HarnessResult<DriveResult> {
        let Some(deferred) = self.find_deferred_handle(operation_id).await? else {
            return Ok(DriveResult {
                operation_id: operation_id.to_string(),
                outcome: DriveOutcome::Idle,
            });
        };
        let result = self.resume_deferred(deferred).await?;
        Ok(DriveResult {
            operation_id: operation_id.to_string(),
            outcome: match result.outcome {
                HarnessRunOutcome::Suspended { deferred, .. } => DriveOutcome::Suspended(deferred),
                _ => DriveOutcome::Completed(Box::new(result)),
            },
        })
    }

    /// `resume(suspendedId)` — resume a parked deferred operation.
    /// `suspended_id` is either the deferred handle id or the entry id of the
    /// suspended assistant message. Mirrors the TS `resume` operation.
    pub async fn resume(&self, suspended_id: &str) -> HarnessResult<RunResult> {
        let Some(deferred) = self.find_deferred_handle(suspended_id).await? else {
            return Err(HarnessError::NothingToResume {
                lane: self.lane.clone(),
                message: format!("no suspended operation matches '{suspended_id}'"),
            });
        };
        self.resume_deferred(deferred).await
    }

    // -- internal helpers for the surface above ------------------------------

    fn active_operation_snapshot(&self) -> Option<ActiveOperationSnapshot> {
        let inner = self.inner.lock().unwrap();
        inner.active_run.as_ref().map(|a| ActiveOperationSnapshot {
            run_id: a.run_id.clone(),
            lane: a.lane.clone(),
            kind: a.kind,
        })
    }

    async fn lane_labels(
        &self,
        view: &Arc<dyn SessionTree>,
    ) -> std::collections::BTreeMap<String, String> {
        let mut labels = std::collections::BTreeMap::new();
        if let Ok(entries) = view
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
        {
            for entry in entries {
                if let Ok(Some(label)) = view.get_label(entry.id()).await {
                    labels.insert(entry.id().to_string(), label);
                }
            }
        }
        labels
    }

    /// Find a parked deferred handle by deferred-id or by the entry id of the
    /// suspended assistant message that carries it.
    async fn find_deferred_handle(
        &self,
        suspended_id: &str,
    ) -> HarnessResult<Option<DeferredHandle>> {
        // Only the current branch's latest assistant may still be parked.
        // Historical handles must not poll again after a terminal answer (or
        // after a newer handle supersedes them).
        let entries = self
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    entry_type: Some("message"),
                    order: Some(EntryOrder::NewestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .map_err(session_to_harness_err)?;
        for entry in entries {
            let Entry::Message(me) = &entry else {
                continue;
            };
            let AgentMessage::Assistant(a) = &me.message else {
                continue;
            };
            if a.stop_reason != StopReason::Deferred {
                return Ok(None);
            }
            let Some(deferred) = &a.deferred else {
                return Ok(None);
            };
            return Ok((deferred.id == suspended_id || entry.id() == suspended_id)
                .then(|| deferred.clone()));
        }
        Ok(None)
    }

    /// Drive a reconstructed deferred handle to a terminal outcome. Kept
    /// separate so `drive`/`resume` share one path.
    ///
    /// Mirrors upstream's `readDeferredSourceHandle` + `publishResponse`: poll
    /// the provider, record what it returned, and then either stay suspended (the
    /// provider handed back another handle) or carry on with the run. "Carry on"
    /// means the whole reason this is not a plain continuation: a polled assistant
    /// message arrives from outside the loop, so its tool calls have **not** been
    /// executed, and the next provider request would be rejected while they are
    /// unanswered — hence the run is entered *from that assistant*.
    async fn resume_deferred(&self, deferred: DeferredHandle) -> HarnessResult<RunResult> {
        let snap = self.snapshot_config()?;
        let provider = snap
            .models
            .iter()
            .find(|provider| provider.id() == deferred.provider)
            .cloned()
            .ok_or_else(|| {
                HarnessError::agent(format!(
                    "no provider '{}' is registered for the deferred handle the run parked on",
                    deferred.provider
                ))
            })?;
        let Some(capability) = provider.deferred() else {
            // Not silently "finished": a provider that cannot poll cannot answer,
            // and pretending otherwise would record a result nobody produced.
            return Err(HarnessError::not_implemented(
                "resume_deferred: this provider has no long-poll continuation",
            ));
        };
        let model = provider
            .models()
            .iter()
            .find(|model| model.id == deferred.model_id)
            .cloned()
            .unwrap_or_else(|| snap.model.clone());

        // Polling owns the same admission/settlement lease as a normal run.
        // Retain the parked operation's identity instead of inventing a second
        // terminal run ID when its final answer arrives.
        let gate = self.message_writes.lock().await;
        if self.find_deferred_handle(&deferred.id).await?.is_none() {
            return Err(HarnessError::NothingToResume {
                lane: self.lane.clone(),
                message: "the deferred handle is no longer current".into(),
            });
        }
        let parked_run_id = self
            .session
            .find_open_operations(&self.lane, Some(1))
            .await
            .map_err(session_to_harness_err)?
            .first()
            .map(|operation| operation.base.id.clone());
        let (admitted_id, signal, _idle) = self.acquire_run_locked(OperationKind::Run)?;
        let run_id = parked_run_id.clone().unwrap_or(admitted_id);
        if let Some(active) = self.inner.lock().unwrap().active_run.as_mut() {
            active.run_id = run_id.clone();
            active.recovering = false;
        }
        let _run_lease = ActiveRunLease {
            harness: self,
            run_id: run_id.clone(),
        };
        drop(gate);
        if parked_run_id.is_none() {
            // Public session APIs can import a deferred message without its
            // original operation. Establish terminal proof before settling it.
            self.write_operation_started(
                &run_id,
                self.get_tip_id().await?,
                OperationIntent::Run {
                    original_prompt: Vec::new(),
                    initial_messages: Vec::new(),
                    system_prompt_override: None,
                    resume_data: None,
                },
            )
            .await?;
        }
        let mut options = self.deferred_stream_options(&snap);
        options.signal = signal;

        let message = capability
            .stream_deferred(&model, &deferred, &options)
            .await
            .result()
            .await
            .map_err(|error| {
                HarnessError::agent(format!("the deferred poll produced no message: {error}"))
            })?;

        // Record the polled message: it is real history (the model wrote it), and
        // a *new* handle it carries has to be discoverable, which is what
        // `find_deferred_handle` reads.
        let entry_id = self.next_entry_id();
        self.append_provisioned(ProvisionedEntry {
            id: entry_id.clone(),
            kind: ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(message.clone())),
                terminate: None,
            },
        })
        .await?;
        let leaf_id = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?
            .unwrap_or_else(|| entry_id.clone());

        // Still not ready: stay suspended. The operation stays open, so the next
        // start (or the next `drive`) polls the new handle in turn.
        if message.stop_reason == StopReason::Deferred {
            return Ok(RunResult {
                run_id,
                outcome: HarnessRunOutcome::Suspended {
                    leaf_id,
                    final_entry_id: entry_id,
                    deferred: message.deferred.clone().unwrap_or(deferred),
                },
            });
        }

        // Anything left to do? A polled message that carries tool calls needs them
        // executed before the next provider request.
        let has_tool_calls = message
            .content
            .iter()
            .any(|block| matches!(block, Content::ToolCall(_)));
        if has_tool_calls && !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
        {
            self.write_operation_finished(&run_id, OperationOutcome::Completed, None)
                .await?;
            // The continuation owns a new native operation and performs the
            // logical run's one final RunEnd after its tools/results settle.
            return self.run_core_from_assistant(&run_id).await;
        }

        let mut outcome = Self::derive_outcome(
            &[AgentMessage::Assistant(Box::new(message))],
            Some(leaf_id),
            Some(entry_id),
        );
        self.finish_run_result(&run_id, &mut outcome).await?;
        Ok(RunResult { run_id, outcome })
    }

    /// The options a deferred poll runs with. Built from the harness snapshot so a
    /// poll sees the same timeout/thinking settings as an ordinary request.
    /// The caller replaces the fresh default token with its admitted run token.
    fn deferred_stream_options(&self, snap: &ConfigSnapshot) -> rpi_ai::SimpleStreamOptions {
        rpi_ai::SimpleStreamOptions {
            timeout: snap.stream_options.timeout,
            stream_idle_timeout: snap.stream_options.stream_idle_timeout,
            session_id: None,
            signal: CancellationToken::new(),
            reasoning: match snap.thinking_level {
                ThinkingLevel::Off => None,
                other => Some(other),
            },
            ..Default::default()
        }
    }

    // -- harness-level config accessors (TS `AgentHarness` class surface, not
    //    on the `AgentLane` interface). Defensive-copy: setters Clone/move in,
    //    getters return clones. ---------------------------------------------

    /// `getTools()`. Mirrors TS `AgentHarness.getTools` — returns a defensive
    /// clone of the tool registry.
    pub async fn get_tools(&self) -> HarnessResult<Vec<HarnessTool>> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.tools.clone())
    }

    /// Seal the host's exact startup selection. Empty means no tools. Resource
    /// mutations require a restart; the legacy defaults are unchanged until sealed.
    pub fn seal_resources(&self, active: Vec<String>) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.exact_resources {
            return Err(HarnessError::io("exact resources already sealed"));
        }
        for name in &active {
            if !inner.tools.iter().any(|t| t.tool.schema().name == *name) {
                return Err(HarnessError::io(format!("unknown selected tool: {name}")));
            }
        }
        inner.active_tool_names = active;
        inner.exact_resources = true;
        Ok(())
    }

    pub fn has_exact_resources(&self) -> bool {
        self.inner.lock().unwrap().exact_resources
    }

    /// `setTools(tools, activeNames?)`. Mirrors TS `AgentHarness.setTools` —
    /// replaces the registry and (when `active_names` is `None`) resets
    /// `active_tool_names` to every tool's schema name. Inputs are moved in
    /// (Rust ownership already isolates them from the caller).
    pub async fn set_tools(
        &self,
        tools: Vec<HarnessTool>,
        active_names: Option<Vec<String>>,
    ) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        if inner.exact_resources {
            return Err(HarnessError::io(
                "exact resources are restart-required; tool registry mutation unsupported",
            ));
        }
        let active = active_names
            .unwrap_or_else(|| tools.iter().map(|t| t.tool.schema().name.clone()).collect());
        inner.tools = tools;
        inner.active_tool_names = active;
        Ok(())
    }

    /// `close()`. Mirrors TS `AgentHarness.close` — flips the closed flag so
    /// every subsequent operation rejects with [`HarnessError::Closed`]. An
    /// in-flight run is not interrupted (its guard holds its own signal); the
    /// flag gates new operations.
    pub async fn close(&self) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        self.child_scope.lock().unwrap().cancel();
        Ok(())
    }

    /// Swap the harness's durable session facade (TUI `/session` hot-switch).
    /// `run_core` re-reads the session on every run (branch path, leaf id,
    /// context build), so replacing `inner.session` makes the next run
    /// continue in the new session file — no harness rebuild, no lane/event
    /// wiring churn. Abort and wait for any in-flight operation before switching;
    /// otherwise this rejects with `LaneBusy` and leaves the old session intact.
    pub async fn set_session(&self, session: Session) -> HarnessResult<()> {
        let _gate = self.message_writes.lock().await;
        {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            if inner.exact_resources {
                return Err(HarnessError::io(
                    "exact resource session switching unsupported; restart required",
                ));
            }
            if let Some(active) = &inner.active_run {
                return Err(HarnessError::lane_busy(
                    self.lane.clone(),
                    active.run_id.clone(),
                    active.kind,
                    "Wait for the active operation to finish before switching sessions",
                ));
            }
        }
        // Prepare everything against a pinned target before publishing the swap.
        // A failed read must leave the old session and its queues untouched.
        let target = session.pin(Arc::clone(self.session.id_generator()));
        let lanes = target.get_lanes().await.map_err(session_to_harness_err)?;
        if !lanes.iter().any(|lane| lane.lane == self.lane) {
            return Err(HarnessError::io(
                "Target session does not contain the bound lane",
            ));
        }
        target
            .prune_completed_progress()
            .await
            .map_err(session_to_harness_err)?;
        let mut rebuilt = Vec::new();
        for lane in lanes {
            rebuilt.extend(Self::rebuild_queues(&target, &lane.lane).await?);
        }
        let mut inner = self.inner.lock().unwrap();
        // Keep the queue handles: existing lane handles share these Arcs.
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.steering_queue.lock().unwrap().pending.clear();
        inner.follow_up_queue.lock().unwrap().pending.clear();
        inner.next_run_queue.lock().unwrap().pending.clear();
        inner.pending_context_lanes.clear();
        inner.pending_resume = None;
        for (kind, item) in rebuilt {
            let queue = match kind {
                QueueKind::Steer => &inner.steering_queue,
                QueueKind::FollowUp => &inner.follow_up_queue,
                QueueKind::NextRun => &inner.next_run_queue,
                QueueKind::Context => {
                    inner.pending_context_lanes.insert(item.lane);
                    continue;
                }
            };
            queue.lock().unwrap().pending.push_back(item);
        }
        // Retire children of the old session, but permit new children after the swap.
        {
            let mut scope = self.child_scope.lock().unwrap();
            scope.cancel();
            *scope = CancellationToken::new();
        }
        self.session.rebind_to(&target);
        Ok(())
    }

    /// `getResources()`. Mirrors TS `AgentHarness.getResources`
    /// (`agent-harness.ts:460-464`) — returns a defensive clone of the
    /// resources (skills + prompt-templates) the harness was built with.
    /// Callers (e.g. the interactive TUI's `/`-autocomplete, which lists
    /// prompt-template names alongside built-in slash commands) read the
    /// discovered set through this accessor without touching the lock-held
    /// inner directly.
    pub async fn get_resources(&self) -> HarnessResult<AgentHarnessResources> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.resources.clone())
    }

    /// `getSystemPrompt()`. Mirrors TS `AgentHarness.getSystemPrompt` — a
    /// defensive clone of the composed base system prompt the harness was built
    /// with (base + append + context; the `<available_skills>` listing is added
    /// at run time from the resource skills, so this returns the *base* prompt
    /// the plugin's `runtime_action(GetSystemPrompt)` sees). B5a: backs the
    /// [`rpi_extensions::RuntimeActionHost::get_system_prompt`] action.
    pub async fn get_system_prompt(&self) -> HarnessResult<Option<String>> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.system_prompt.clone())
    }

    // ---- B5d: reload mutation setters -------------------------------------
    //
    // `/reload` re-runs skill/prompt/context/SYSTEM discovery and must push the
    // rebuilt state into a *live* harness without rebuilding it (rebuilding
    // would tear down the session/lane/event wiring). These setters mirror
    // `set_tools`/`set_session`: take the lock, reject if closed, assign, drop.
    // The next `run_core` snapshots the new values via `snapshot_config`, so a
    // mutation here takes effect on the NEXT run — exactly the contract pi's
    // `/reload` offers (in-flight runs finish on the old config; the reloaded
    // resources apply to the subsequent turn).
    //
    // All five are `pub async` (uniform with the existing setters) even though
    // the bodies are sync; the `async` keeps the call sites uniform with
    // `set_tools`/`set_session` and leaves room for future emission (e.g. a
    // `ResourcesChanged` harness event) without an API break.

    /// `setSystemPrompt(prompt)`. Replace the composed base system prompt the
    /// harness was built with. B5d `/reload` calls this after recomposing
    /// (base + context + append); the next run's `snapshot_config` picks it up.
    pub async fn set_system_prompt(&self, prompt: Option<String>) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        if inner.exact_resources {
            return Err(HarnessError::io("exact resources are restart-required"));
        }
        inner.system_prompt = prompt;
        Ok(())
    }

    /// `setResources(resources)`. Replace the skills + prompt-templates the
    /// harness advertises (TUI `/context` listing, `<available_skills>` gate).
    pub async fn set_resources(&self, resources: AgentHarnessResources) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        if inner.exact_resources {
            return Err(HarnessError::io("exact resources are restart-required"));
        }
        inner.resources = resources;
        Ok(())
    }
    /// `setAgentEmitter(emitter)`. Replace the emitter override (B3a's
    /// `ExtensionEmitter` fans `AgentEvent`s to plugin `on()` handlers). On
    /// `/reload` the old emitter is dropped (unsubscribing from the broadcast)
    /// and a fresh one over the reloaded registry takes its place.
    pub async fn set_agent_emitter(
        &self,
        emitter: Option<Arc<dyn AgentEmitter>>,
    ) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.agent_emitter = emitter;
        Ok(())
    }

    /// `setModels(models)`. Replace the provider registry. B5d `/reload`
    /// rebuilds `vec![gateway] + PluggableProvider::from_session(&new_session)`
    /// and installs it here so the next run resolves provider plugins from the
    /// reloaded registry (the old `ActionBridge`/`ExtensionSession` are
    /// invalidated; their `PluggableProvider`s reject calls via the staleness
    /// guard).
    pub async fn set_models(&self, models: Vec<Arc<dyn AiProvider>>) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.models = models;
        Ok(())
    }

    /// `setProviderHooks(hooks)`. Replace the per-call provider hooks (B4's
    /// `ExtensionProviderHooks` fires `before_request`/`after_response` inside
    /// the `StreamFn` closure). `/reload` builds a fresh hooks adapter over the
    /// reloaded registry snapshot.
    pub async fn set_provider_hooks(
        &self,
        hooks: Option<Arc<dyn rpi_ai::ProviderHooks>>,
    ) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.provider_hooks = hooks;
        Ok(())
    }

    // -- private helpers ----------------------------------------------------

    /// Reject if closed or if a lane already has an active operation. On
    /// success, installs the `ActiveRun` guard and returns `(run_id, signal,
    /// idle_notify)`.
    async fn acquire_run(
        &self,
        kind: OperationKind,
    ) -> HarnessResult<(String, CancellationToken, Arc<tokio::sync::Notify>)> {
        let _gate = self.message_writes.lock().await;
        self.acquire_run_locked(kind)
    }

    /// Caller holds `message_writes` while deciding whether to start a run.
    fn acquire_run_locked(
        &self,
        kind: OperationKind,
    ) -> HarnessResult<(String, CancellationToken, Arc<tokio::sync::Notify>)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        if let Some(active) = &inner.active_run {
            return Err(HarnessError::lane_busy(
                self.lane.clone(),
                active.run_id.clone(),
                active.kind,
                format!(
                    "Lane {} already has an active {} operation",
                    self.lane,
                    active.kind.as_str()
                ),
            ));
        }
        // Clear a request left over from an *idle* call. The documented contract
        // is that `StopRound` on an idle lane is a no-op, and without this the
        // flag would be consumed by the next run's first turn boundary — i.e.
        // the next prompt would stop after one turn as if the user had asked.
        //
        // This has to happen *here*, while `message_writes` and `inner` are
        // both held and before `active_run` becomes visible: once the run is
        // advertised, a caller may legitimately issue `StopRound` for it, and a
        // reset that lands afterwards would erase exactly that request.
        self.stop_round
            .store(false, std::sync::atomic::Ordering::SeqCst);

        let run_id = self.session.id_generator().next();
        let signal = CancellationToken::new();
        let idle = Arc::new(tokio::sync::Notify::new());
        inner.active_run = Some(ActiveRun {
            finishing: false,
            recovering: true,
            run_id: run_id.clone(),
            lane: self.lane.clone(),
            signal: signal.clone(),
            children: self.child_scope.lock().unwrap().child_token(),
            idle: Arc::clone(&idle),
            kind,
        });
        Ok((run_id, signal, idle))
    }

    /// Release the active-run guard and notify idle waiters.
    fn release_run(&self, run_id: &str) {
        let idle = {
            let mut inner = self.inner.lock().unwrap();
            if inner
                .active_run
                .as_ref()
                .is_some_and(|run| run.run_id == run_id)
            {
                inner.active_run.take().map(|a| {
                    // Every admitted run owns its tools, including a root whose
                    // caller drops the foreground future. The loop drops owned
                    // execute futures before this lease releases admission.
                    a.signal.cancel();
                    a.children.cancel();
                    a.idle
                })
            } else {
                None
            }
        };
        if let Some(idle) = idle {
            idle.notify_waiters();
        }
    }

    /// Enqueue a message into a steering/follow-up queue, **requiring an active run**.
    ///
    /// This implementation is now **unused** after aligning with upstream's
    /// design: `steer()` and `follow_up()` now enqueue unconditionally,
    /// mirroring the TS Agent class and avoiding the race between `activeRun`
    /// clearing and the TUI status check. The function is kept for reference
    /// and for potential future queue kinds that may need the active-run guard.
    #[allow(dead_code)]
    /// Provisioned JSON for an entry — the shape a queue record stores as its
    /// `target`. Built through the reducer's own helper so the recorded intent
    /// and the eventual commit cannot drift.
    fn provisioned_target(entry: &ProvisionedEntry) -> JsonValue {
        let placeholder = crate::session::types::provisioned_into_entry(entry.clone(), 0, None, 0);
        crate::session::reducer::entry_provisioned_json(&placeholder)
    }

    /// Recover the queued message from a recorded `target`.
    ///
    /// The target is the entry's flat JSON minus the storage-assigned fields, so
    /// those three are put back with placeholders before deserializing.
    fn message_from_target(target: &JsonValue) -> Option<AgentMessage> {
        let mut object = target.as_object()?.clone();
        object.insert("seq".to_string(), JsonValue::from(0));
        object.insert("parentId".to_string(), JsonValue::Null);
        object.insert("timestamp".to_string(), JsonValue::from(0));
        let entry: Entry = serde_json::from_value(JsonValue::Object(object)).ok()?;
        match crate::session::types::provisioned_from_entry(&entry).kind {
            ProvisionedKind::Message { message, .. } => Some(message),
            _ => None,
        }
    }

    /// The entry id a queue record's `target` names.
    fn target_entry_id(target: &JsonValue) -> Option<&str> {
        target.get("id").and_then(|value| value.as_str())
    }

    /// Bind manual skill reads to the host's execution environment (including
    /// remote/read routing). Sealed resources use their validated snapshot instead.
    /// No local filesystem fallback is used. Lane handles share this binding;
    /// independent child harnesses must inherit it explicitly.
    pub fn set_skill_execution_env(&self, env: Arc<dyn rpi_tools::ExecutionEnv>) {
        self.inner.lock().unwrap().skill_execution_env = Some(env);
    }

    async fn expand_skill_input(
        &self,
        mut message: AgentMessage,
        signal: Option<&CancellationToken>,
    ) -> AgentMessage {
        let AgentMessage::User(user) = &mut message else {
            return message;
        };
        // A command belongs to the whole user input, not arbitrary substrings
        // or later text blocks. Keep images and all other blocks untouched.
        let text = match &mut user.content {
            UserContent::Text(text) => Some(text),
            UserContent::Blocks(blocks) => blocks.iter_mut().find_map(|block| match block {
                Content::Text(text) => Some(&mut text.text),
                _ => None,
            }),
        };
        let Some(text) = text else { return message };
        let Some((name, args)) = parse_skill_command(text) else {
            return message;
        };
        let (skill, env, exact_resources) = {
            let inner = self.inner.lock().unwrap();
            (
                inner
                    .resources
                    .skills
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .find(|skill| skill.name == name)
                    .cloned(),
                inner.skill_execution_env.clone(),
                inner.exact_resources,
            )
        };
        let Some(skill) = skill else { return message };
        if exact_resources {
            // Exact startup selection validated and pinned this body. Rereading
            // here would bypass its hash/lease checks and restart-only contract.
            *text = format_skill_invocation(&skill, Some(args));
            return message;
        }
        let Some(env) = env else {
            tracing::warn!(path = %skill.file_path, event = "skill_expansion",
                "Cannot expand skill command without an execution environment");
            return message;
        };
        match env
            .read_text_file(&skill.file_path, signal)
            .await
            .map_err(|error| error.to_string())
            .and_then(|source| format_skill_command(&skill, &source, args))
        {
            Ok(expanded) => *text = expanded,
            Err(error) => tracing::warn!(path = %skill.file_path, event = "skill_expansion",
                error = %error, "Could not expand skill command"),
        }
        message
    }

    /// Enqueue a message and record the intent durably.
    ///
    /// The record is what lets a queued message survive a crash: the in-memory
    /// queue is rebuilt from these records on the next start. Without it the
    /// queue is purely process-local, so a steer typed while the agent works is
    /// simply lost.
    async fn enqueue_message(
        &self,
        message: AgentMessage,
        kind: QueueKind,
    ) -> HarnessResult<QueueResult> {
        let message = self.expand_skill_input(message, None).await;
        let gate = self.message_writes.clone().lock_owned().await;
        self.enqueue_message_locked(message, kind, gate).await
    }

    /// Publish to the in-memory queue only after the durable enqueue succeeds.
    /// Caller holds `message_writes`, shared with queue drains.
    async fn enqueue_message_locked(
        &self,
        message: AgentMessage,
        kind: QueueKind,
        gate: tokio::sync::OwnedMutexGuard<()>,
    ) -> HarnessResult<QueueResult> {
        let harness = self.clone();
        tokio::spawn(async move {
            let _gate = gate;
            let (entry_id, queue) = {
                let inner = harness.inner.lock().unwrap();
                if inner.closed {
                    return Err(HarnessError::closed());
                }
                let queue = match kind {
                    QueueKind::Steer => Arc::clone(&inner.steering_queue),
                    QueueKind::FollowUp => Arc::clone(&inner.follow_up_queue),
                    QueueKind::NextRun => Arc::clone(&inner.next_run_queue),
                    QueueKind::Context => unreachable!("context appends use the boundary writer"),
                };
                (harness.session.id_generator().next(), queue)
            };
            harness
                .write_queue_enqueued(kind, &entry_id, &message)
                .await?;
            queue.lock().unwrap().pending.push_back(QueuedMessage {
                entry_id: entry_id.clone(),
                lane: harness.lane.clone(),
                message,
            });
            Ok(QueueResult { entry_id })
        })
        .await
        .map_err(|error| HarnessError::io(format!("queue enqueue task failed: {error}")))?
    }

    /// Record that `entry_id` is queued.
    async fn write_queue_enqueued(
        &self,
        kind: QueueKind,
        entry_id: &str,
        message: &AgentMessage,
    ) -> HarnessResult<()> {
        let target = Self::provisioned_target(&ProvisionedEntry {
            id: entry_id.to_string(),
            kind: ProvisionedKind::Message {
                message: message.clone(),
                terminate: None,
            },
        });
        self.session
            .append_record(LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
                base: self.record_base(),
                queue: kind,
                // Queue acceptance belongs to the lane. It can precede the
                // operation's durable start or survive its completion.
                run_id: None,
                target,
            }))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Find one pending enqueue in this lane; do not hide storage failures.
    async fn find_enqueue(&self, entry_id: &str) -> HarnessResult<Option<QueueEnqueuedRecord>> {
        let records = self
            .session
            .find_records(&RecordQuery {
                lane: Some(self.lane.clone()),
                record_type: Some("queue_enqueued"),
                target_entry_id: Some(entry_id.to_owned()),
                pending_queue_only: true,
                limit: Some(1),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)?;
        Ok(records.into_iter().find_map(|record| match record {
            LaneRecord::QueueEnqueued(enqueued) => Some(enqueued),
            _ => None,
        }))
    }

    /// Take every queued item for this lane the way the loop's injection
    /// callbacks need: record the consumption, and hand the reserved entry id to
    /// the settle observer so the injected message is committed when it settles
    /// rather than surviving only until the run ends.
    async fn drain_injected(
        &self,
        queue: SharedMessageQueue,
        settle: &Arc<crate::settle::SettleState>,
    ) -> Vec<AgentMessage> {
        let _gate = self.message_writes.lock().await;
        let reservation = QueueReservation::reserve(queue, &self.lane, false);
        let mut messages = Vec::with_capacity(reservation.items.len());
        for item in &reservation.items {
            settle.push_injected(item.entry_id.clone(), item.message.clone());
            messages.push(item.message.clone());
        }
        settle.hold_injected_reservation(reservation);
        messages
    }

    /// Rebuild the in-memory queues from the durable records.
    ///
    /// An item is still queued when its enqueue has no matching cancellation.
    /// Reconstructed in recorded order, so a restart delivers what was pending in
    /// the order the user wrote it.
    async fn rebuild_queues(
        session: &Session,
        lane: &str,
    ) -> HarnessResult<Vec<(QueueKind, QueuedMessage)>> {
        let diagnostics = crate::session::diagnostics::RecoveryPhase::start("queue_rebuild");
        // Filter before cloning payloads. A single backend read also avoids
        // merging enqueue/cancel snapshots from different points in time.
        let records = session
            .find_records(&RecordQuery {
                lane: Some(lane.to_string()),
                record_type: Some("queue_enqueued"),
                pending_queue_only: true,
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)?;
        let selected = records.len();
        let mut rebuilt = Vec::new();
        for record in records {
            let LaneRecord::QueueEnqueued(enqueued) = record else {
                continue;
            };
            let Some(entry_id) = Self::target_entry_id(&enqueued.target) else {
                continue;
            };
            let Some(message) = Self::message_from_target(&enqueued.target) else {
                continue;
            };
            rebuilt.push((
                enqueued.queue,
                QueuedMessage {
                    entry_id: entry_id.to_string(),
                    lane: lane.to_string(),
                    message,
                },
            ));
        }
        diagnostics.finish(selected, rebuilt.len());
        Ok(rebuilt)
    }

    /// Startup repair must never bypass a host authorization hook or wait for
    /// an interactive UI before it exists. Unknown outcomes are safe to report;
    /// any new execution can be requested through the normal authorized loop.
    fn recovery_tools(&self) -> Vec<HarnessTool> {
        let inner = self.inner.lock().unwrap();
        if inner.before_tool_call.is_some() {
            Vec::new()
        } else {
            inner.tools.clone()
        }
    }

    /// A record base for this lane. The store re-stamps `seq`/`lane`/`timestamp`,
    /// but the lane must still name a real one: it is how the store routes the
    /// append.
    fn record_base(&self) -> RecordBase {
        RecordBase {
            id: self.session.id_generator().next(),
            seq: 0,
            lane: self.lane.clone(),
            timestamp: 0,
        }
    }

    fn ensure_recovery_active(signal: &CancellationToken) -> HarnessResult<()> {
        if signal.is_cancelled() {
            return Err(HarnessError::agent(
                "Interrupted-operation recovery was cancelled",
            ));
        }
        Ok(())
    }

    /// Repair one interrupted run incrementally. Every message has a stable
    /// identity and commits before the next tool is considered, so recovery can
    /// itself be interrupted without losing output or duplicating entries.
    async fn repair_interrupted_run(
        session: &Session,
        lane: &str,
        run_id: &str,
        tools: &[HarnessTool],
        signal: &CancellationToken,
        salvage_failed_attempts: bool,
    ) -> HarnessResult<()> {
        Self::ensure_recovery_active(signal)?;
        let records = session
            .find_records(&RecordQuery {
                lane: Some(lane.to_owned()),
                record_type: Some("tool_started"),
                run_id: Some(run_id.to_owned()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)?;
        let started: Vec<_> = records
            .into_iter()
            .filter_map(|record| match record {
                LaneRecord::ToolStarted(record) => Some(record),
                _ => None,
            })
            .collect();

        // Finish the durable tail's tool batch BEFORE appending any newer
        // stream. Otherwise even a synthetic abort stream can hide an older
        // missing result behind a new assistant/context boundary. Extension
        // state is not such a boundary.
        let entries = session
            .view(lane)
            .find_entries_on_branch(
                &EntryQuery {
                    exclude_tool_program_bodies: true,
                    order: Some(EntryOrder::NewestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .map_err(session_to_harness_err)?;
        for entry in entries {
            match entry {
                Entry::Custom(_) => continue,
                Entry::Message(message) => match message.message {
                    AgentMessage::ToolResult(_) => continue,
                    AgentMessage::Assistant(partial) => {
                        Self::repair_tool_batch(
                            session,
                            lane,
                            tools,
                            signal,
                            &started,
                            &message.base.id,
                            &partial,
                        )
                        .await?;
                        break;
                    }
                    _ => break,
                },
                _ => break,
            }
        }

        if salvage_failed_attempts {
            for stream in crate::frame_progress::replay_run_streams(session, run_id).await? {
                Self::ensure_recovery_active(signal)?;
                let entry_id = stream
                    .entry_id
                    .unwrap_or_else(|| session.id_generator().next());
                crate::frame_progress::bind_stream_entry(
                    session,
                    lane,
                    run_id,
                    stream.stream_index,
                    &entry_id,
                )
                .await?;
                let partial = crate::frame_progress::interrupted_message(stream.message);
                session
                    .append_entry(
                        ProvisionedEntry {
                            id: entry_id.clone(),
                            kind: ProvisionedKind::Message {
                                message: AgentMessage::Assistant(Box::new(partial.clone())),
                                terminate: None,
                            },
                        },
                        lane,
                    )
                    .await
                    .map_err(session_to_harness_err)?;
                Self::repair_tool_batch(
                    session, lane, tools, signal, &started, &entry_id, &partial,
                )
                .await?;
            }
        }
        Self::ensure_recovery_active(signal)
    }

    async fn repair_tool_batch(
        session: &Session,
        lane: &str,
        tools: &[HarnessTool],
        signal: &CancellationToken,
        started: &[ToolStartedRecord],
        assistant_entry_id: &str,
        partial: &AssistantMessage,
    ) -> HarnessResult<()> {
        let recorded: BTreeMap<_, _> = started
            .iter()
            .filter(|record| record.assistant_entry_id == assistant_entry_id)
            .map(|record| (record.tool_call_id.clone(), record.clone()))
            .collect();
        for outcome in crate::frame_progress::unknown_tool_outcomes_with_started(
            session,
            lane,
            Some(assistant_entry_id),
            partial,
            started,
        )
        .await?
        {
            Self::ensure_recovery_active(signal)?;
            // A started call already reserved its result. For calls that had not
            // started, use an unambiguous identity scoped to this assistant.
            let id = recorded
                .get(&outcome.tool_call_id)
                .map(|record| record.result_entry_id.clone())
                .unwrap_or_else(|| {
                    format!(
                        "recovery:{}:{}:{}:{}",
                        assistant_entry_id.len(),
                        assistant_entry_id,
                        outcome.tool_call_id.len(),
                        outcome.tool_call_id
                    )
                });
            if session
                .get_entry(&id)
                .await
                .map_err(session_to_harness_err)?
                .is_some()
            {
                return Err(HarnessError::io(format!(
                    "Recovery result {id} exists outside its tool batch"
                )));
            }
            let message =
                Self::resolve_tool_call(tools, signal, &recorded, &outcome, partial.timestamp)
                    .await?;
            session
                .append_entry(
                    ProvisionedEntry {
                        id,
                        kind: ProvisionedKind::Message {
                            message,
                            terminate: None,
                        },
                    },
                    lane,
                )
                .await
                .map_err(session_to_harness_err)?;
        }
        Ok(())
    }

    /// Whether the branch tip is a user message the run died on.
    ///
    /// That is the `starting` crash: the operation opened, the prompt was
    /// persisted, and the process died before the first provider call. Nothing
    /// was produced, so the tip is still the prompt — and the run should simply
    /// continue, which is what upstream does by re-entering its `starting` state.
    async fn tip_is_user_message(session: &Session, lane: &str) -> bool {
        let entries = match session
            .view(lane)
            .find_entries_on_branch(
                &EntryQuery {
                    exclude_tool_program_bodies: true,
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
        {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        // Extension state (for example durable's AgentStart journal) can be
        // appended after the prompt without advancing the conversation. Match
        // lane_is_resumable: inspect the last non-custom entry, not an arbitrary
        // older user message hidden behind an assistant reply.
        let tip = entries
            .iter()
            .rev()
            .find(|entry| !matches!(entry, Entry::Custom(_)));
        let Some(Entry::Message(message)) = tip else {
            return false;
        };
        matches!(message.message, AgentMessage::User(_))
            && Self::queued_entry_kind_checked(session, lane, &message.base.id)
                .await
                .unwrap_or(None)
                != Some(QueueKind::Context)
    }

    /// Whether the branch tip is an assistant message.
    async fn tip_is_assistant(session: &Session, lane: &str) -> bool {
        let entries = match session
            .view(lane)
            .find_entries_on_branch(
                &EntryQuery {
                    exclude_tool_program_bodies: true,
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
        {
            Ok(entries) => entries,
            Err(_) => return true,
        };
        matches!(
            entries.iter().rev().find(|entry| !matches!(entry, Entry::Custom(_))),
            Some(Entry::Message(message)) if matches!(message.message, AgentMessage::Assistant(_))
        )
    }

    /// Resolve one unresolved tool call: replay it when that is safe, otherwise
    /// record that its outcome is unknown.
    async fn resolve_tool_call(
        tools: &[HarnessTool],
        signal: &CancellationToken,
        recorded: &std::collections::BTreeMap<String, ToolStartedRecord>,
        outcome: &crate::frame_progress::UnknownToolOutcome,
        timestamp: i64,
    ) -> HarnessResult<AgentMessage> {
        Self::ensure_recovery_active(signal)?;
        // A call that already ran upstream is answered from its recording and is
        // never replayed, whatever the tool declares about replay.
        if let Some(recording) = &outcome.recorded {
            return Ok(crate::frame_progress::recorded_tool_result(
                outcome, recording, timestamp,
            ));
        }
        let frame = recorded.get(&outcome.tool_call_id);
        let tool = tools
            .iter()
            .find(|tool| tool.tool.schema().name == outcome.tool_name);
        // Dual confirmation, exactly like native: the call must have been
        // recorded as safe *and* the tool must still declare itself safe.
        // Either alone would replay calls that are not safe to repeat.
        let may_replay = frame.is_some_and(|frame| frame.replay == ToolReplay::Safe)
            && tool.is_some_and(|tool| tool.replay == crate::types::ToolReplay::Safe);
        if !may_replay {
            return Ok(crate::frame_progress::interrupted_tool_result(
                outcome, timestamp,
            ));
        }
        let (Some(frame), Some(tool)) = (frame, tool) else {
            return Ok(crate::frame_progress::interrupted_tool_result(
                outcome, timestamp,
            ));
        };

        // Do not detach/select away an entered call: its actual outcome must
        // land before cancellation is propagated at the next recovery boundary.
        Self::ensure_recovery_active(signal)?;
        let replayed = tool
            .tool
            .execute(
                &outcome.tool_call_id,
                frame.effective_args.clone(),
                signal.child_token(),
                Arc::new(|_| {}),
            )
            .await
            .map_err(|error| error.to_string());
        Ok(match replayed {
            Ok(result) => {
                // The tool's own output first, then the marker: mirrors upstream's
                // `interruptedOutcome`, which appends its marker after the content
                // the tool had already produced, and keeps the result readable —
                // the reader sees the output before the note about it.
                let mut content = result.clone().into_content();
                content.push(Content::text(crate::tool_recovery::REPLAYED_TOOL_RESULT));
                AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
                    role: rpi_ai::types::ToolResultRole,
                    tool_call_id: outcome.tool_call_id.clone(),
                    tool_name: outcome.tool_name.clone(),
                    content,
                    details: Some(result.details.clone()),
                    usage: result.usage.clone(),
                    added_tool_names: result.added_tool_names.clone(),
                    is_error: false,
                    timestamp,
                }))
            }
            Err(error) => AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
                role: rpi_ai::types::ToolResultRole,
                tool_call_id: outcome.tool_call_id.clone(),
                tool_name: outcome.tool_name.clone(),
                content: vec![Content::text(format!(
                    "{}\n\nThe re-run failed: {error}",
                    crate::tool_recovery::REPLAYED_TOOL_RESULT
                ))],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: true,
                timestamp,
            })),
        })
    }

    /// Settle an operation left open by a process that died, repairing what it
    /// left behind.
    ///
    /// Successful repair closes the old operation. Failure or cancellation leaves
    /// it open for a later attempt. Safe tool replays obey the existing policy;
    /// this settlement never starts a provider request or continuation itself.
    /// When repair leaves the branch continuable, `create` records a
    /// [`PendingResume`] so the run can be picked up again.
    async fn finish_interrupted_operation(
        session: &Session,
        lane: &str,
        tools: &[HarnessTool],
        signal: &CancellationToken,
        salvage_failed_attempts: bool,
        write_failures: Option<&Mutex<Vec<HarnessWriteFailure>>>,
    ) -> HarnessResult<()> {
        let open = session
            .find_open_operations(lane, Some(2))
            .await
            .map_err(session_to_harness_err)?;
        if open.len() > 1 {
            return Err(HarnessError::io(format!(
                "Lane {lane} has multiple open operations; the session log is corrupted"
            )));
        }
        let Some(operation) = open.first() else {
            return Ok(());
        };
        let run_id = operation.base.id.clone();

        if let Err(error) = Self::repair_interrupted_run(
            session,
            lane,
            &run_id,
            tools,
            signal,
            salvage_failed_attempts,
        )
        .await
        {
            match (&error, write_failures) {
                // User cancellation is not a refused session write.
                (HarnessError::Agent { .. }, _) | (_, None) => {}
                (_, Some(queue)) => {
                    record_write_failure(queue, "repairing an interrupted run", &error.to_string())
                }
            }
            return Err(error);
        }

        // All recovered entries are now durable. A failed retirement leaves the
        // operation open for retry; a later terminal-write failure is also safe
        // because transcript identity/bindings no longer depend on payloads.
        crate::frame_progress::retire_run_frames(session, lane, &run_id).await?;
        Self::ensure_recovery_active(signal)?;
        // This terminal record is the recovery commit barrier, not its intent.
        session
            .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                base: RecordBase {
                    id: session.id_generator().next(),
                    seq: 0,
                    lane: lane.to_string(),
                    timestamp: 0,
                },
                run_id: run_id.clone(),
                outcome: OperationOutcome::Aborted,
                result: None,
                error: Some(OperationError {
                    code: "interrupted".to_string(),
                    message: "Operation was interrupted before it could finish".to_string(),
                }),
            }))
            .await
            .map_err(session_to_harness_err)?;

        Ok(())
    }

    /// Resolve the provider for `model.provider`. Returns the first registered
    /// provider whose `id()` matches.
    fn resolve_provider(
        models: &[Arc<dyn AiProvider>],
        model: &Model,
    ) -> HarnessResult<Arc<dyn AiProvider>> {
        models
            .iter()
            .find(|p| p.id() == model.provider)
            .cloned()
            .ok_or_else(|| {
                HarnessError::agent(format!(
                    "No provider registered for model provider '{}'",
                    model.provider
                ))
            })
    }

    /// Build a `StreamFn` that bridges the sync-return contract to the async
    /// `Provider::stream_simple`. Mirrors `examples/minimal`'s `make_stream_fn`:
    /// `block_in_place` + `Handle::current().block_on`.
    ///
    /// B4: the closure also captures `provider_hooks`; before each
    /// `stream_simple` it fires `before_request` and applies the returned
    /// `SimpleStreamOptionsPatch` to a clone of `opts`, so an extension's
    /// `before_provider_request`/`before_provider_headers` `on()` handlers can
    /// patch the LIVE per-call options (not a run-once config-build — that
    /// would not be equivalent to pi's `beforeRequest`, which fires before
    /// every provider call). `after_response` observation is wired on the
    /// emitter side (B3+) where the terminal `MessageEnd` is visible without
    /// stealing the stream from the loop; the per-call patch is the
    /// load-bearing B4 deliverable here.
    fn build_stream_fn(
        models: Vec<Arc<dyn AiProvider>>,
        provider_hooks: Option<Arc<dyn rpi_ai::ProviderHooks>>,
        stream_options: AgentHarnessStreamOptions,
    ) -> HarnessResult<StreamFn> {
        // Resolve the provider lazily per call (the model may change between
        // turns via prepare_next_turn). If no provider matches, emit an Error
        // terminal event on a synthetic stream.
        let stream_fn = rpi_agent::stream_fn(
            move |model: &Model,
                  ctx: &rpi_ai::types::Context,
                  opts: &rpi_ai::SimpleStreamOptions| {
                let provider = models.iter().find(|p| p.id() == model.provider).cloned();
                match provider {
                    Some(p) => {
                        let model = model.clone();
                        // Hooks usually observe the context without replacing it.
                        // Keep the history borrowed until an actual patch needs
                        // a private mutable copy for this request.
                        let mut ctx = std::borrow::Cow::Borrowed(ctx);
                        // B4 [merge_into]: the loop builds `opts` via
                        // `AgentLoopConfig::to_stream_options`, which DROPS the
                        // harness's pinned `headers`/`metadata` (AgentLoopConfig
                        // has no such fields → to_stream_options sets them None).
                        // Fold the pinned `AgentHarnessStreamOptions` into the
                        // live opts here, at the per-call site, so headers/metadata
                        // reach the provider — then layer the per-call
                        // `ProviderHooks::before_request` patch on top.
                        let mut opts = stream_options.merge_into(opts.clone());
                        if let Some(hooks) = &provider_hooks {
                            // pi's `transform_context`: the last chance to change
                            // what the model reads. Applied to `ctx` before the
                            // request, so a plugin's forced prompt is what this
                            // request carries — and only this one, since `ctx` is
                            // rebuilt per call from the loop's context.
                            if let Some(patch) = hooks.transform_context(&model, &ctx) {
                                if patch != rpi_ai::ContextPatch::default() {
                                    patch.apply(ctx.to_mut());
                                }
                            }
                            if let Some(patch) = hooks.before_request(&model, &ctx, &opts) {
                                patch.apply(&mut opts);
                            }
                        }
                        tokio::task::block_in_place(|| {
                            Handle::current()
                                .block_on(async move { p.stream_simple(&model, &ctx, &opts).await })
                        })
                    }
                    None => {
                        // No provider: synthesize an Error terminal stream.
                        let (mut producer, stream) =
                            rpi_ai::event_stream::create_assistant_message_event_stream();
                        let msg = AssistantMessage::terminal(
                            model.api.clone(),
                            &model.provider,
                            &model.id,
                            StopReason::Error,
                            format!("No provider registered for '{}'", model.provider),
                            0,
                        );
                        // Terminal publication never waits for ordinary queue capacity.
                        let _ = futures::executor::block_on(producer.push(
                            rpi_ai::types::AssistantMessageEvent::Error {
                                reason: rpi_ai::types::ErrorReason::Error,
                                error: msg,
                            },
                        ));
                        stream
                    }
                }
            },
        );
        Ok(stream_fn)
    }

    /// Build the `ConvertToLlm` Arc for `AgentLoopConfig`: use the caller's
    /// override if provided, else wrap the harness-level `convert_to_llm`.
    fn build_convert_to_llm(to_provider_messages: Option<ConvertToLlm>) -> ConvertToLlm {
        to_provider_messages.unwrap_or_else(|| {
            Arc::new(|messages: Vec<AgentMessage>| {
                let out = harness_convert_to_llm(messages);
                Box::pin(async move { out })
            })
        })
    }

    /// Build the `SessionContextBuildOptions` from the entry projectors + the
    /// caller-supplied context-entry transforms (B3b). Previously
    /// `entry_transforms` was hardcoded to `Vec::new()`; the options field now
    /// flows through so a host (pi-cli, via rpi-extensions) can inject transforms.
    fn build_opts(
        projectors: &BTreeMap<String, CustomEntryContextMessageProjector>,
        entry_transforms: &[ContextEntryTransform],
    ) -> SessionContextBuildOptions {
        SessionContextBuildOptions {
            entry_transforms: entry_transforms.to_vec(),
            entry_projectors: projectors.clone(),
        }
    }

    /// Load this lane's branch path, oldest-first. Returns an empty vec when
    /// the lane is empty (no leaf).
    async fn branch_path_oldest_first(&self) -> HarnessResult<Vec<Entry>> {
        let diagnostics =
            std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0");
        let started = diagnostics.then(std::time::Instant::now);
        let leaf = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;
        let Some(leaf_id) = leaf else {
            return Ok(Vec::new());
        };
        let query = EntryQuery {
            exclude_tool_program_bodies: true,
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        };
        let bounds = BranchBounds {
            start: Some(leaf_id),
            ..Default::default()
        };
        let result = self
            .session
            .view(&self.lane)
            .find_entries_on_branch(&query, &bounds)
            .await
            .map_err(session_to_harness_err);
        if let Some(started) = started {
            eprintln!(
                "RPI_CONTEXT_DIAGNOSTICS path_load_ns={} path_entries={}",
                started.elapsed().as_nanos().min(u64::MAX as u128),
                result.as_ref().map(Vec::len).unwrap_or_default(),
            );
        }
        result
    }

    /// Load the default context through the storage binding. This avoids
    /// materializing the complete branch; storage validates the same parent
    /// chain and clones only model-visible messages.
    async fn branch_context_default(
        &self,
    ) -> HarnessResult<crate::session::context::SessionContext> {
        self.session
            .context_on_branch_for_lane(&self.lane, &BranchBounds::default())
            .await
            .map_err(session_to_harness_err)
    }

    /// Snapshot the inner config fields needed to build an `AgentLoopConfig`.
    fn snapshot_config(&self) -> HarnessResult<ConfigSnapshot> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(ConfigSnapshot {
            child_scope: inner.active_run.as_ref().map_or_else(
                || self.child_scope.lock().unwrap().child_token(),
                |run| run.children.child_token(),
            ),
            parent_signal: inner.active_run.as_ref().map(|run| run.signal.clone()),
            model: inner.model.clone(),
            thinking_level: inner.thinking_level,
            active_tool_names: if !inner.exact_resources && inner.active_tool_names.is_empty() {
                inner
                    .tools
                    .iter()
                    .map(|t| t.tool.schema().name.clone())
                    .collect()
            } else {
                inner
                    .active_tool_names
                    .iter()
                    .filter(|name| inner.tools.iter().any(|t| t.tool.schema().name == **name))
                    .cloned()
                    .collect()
            },
            tools: inner.tools.clone(),
            system_prompt: inner.system_prompt.clone(),
            resources: inner.resources.clone(),
            stream_options: inner.stream_options.clone(),
            retry: inner.retry.clone(),
            compaction: inner.compaction,
            tool_execution: inner.tool_execution,
            driving: inner.driving,
            models: inner.models.clone(),
            to_provider_messages: inner.to_provider_messages.clone(),
            entry_projectors: inner.entry_projectors.clone(),
            entry_transforms: inner.entry_transforms.clone(),
            agent_emitter: inner.agent_emitter.clone(),
            before_tool_call: inner.before_tool_call.clone(),
            after_tool_call: inner.after_tool_call.clone(),
            transform_context: inner.transform_context.clone(),
            provider_hooks: inner.provider_hooks.clone(),
            steering_queue: Arc::clone(&inner.steering_queue),
            follow_up_queue: Arc::clone(&inner.follow_up_queue),
        })
    }

    /// Filter an already normalized execution selection. Legacy empty-means-all
    /// is resolved in snapshot_config, before both prompt and schema construction.
    fn active_tools(tools: &[HarnessTool], active: &[String]) -> Vec<Arc<dyn AgentTool>> {
        tools
            .iter()
            .filter(|t| active.contains(&t.tool.schema().name))
            .map(|t| crate::image_tool::wrap(Arc::clone(&t.tool)))
            .collect()
    }

    /// Compose the system prompt: base prompt + skills listing. The base prompt
    /// the CLI hands the harness already carries the `append` + `context`
    /// sections (folded in by `session.rs` via `compose_system_prompt` before
    /// `AgentHarness::create`), so here the harness only appends the
    /// `<available_skills>` listing — the one section it owns itself.
    ///
    /// **Read-tool gate** (pi `skills.ts:335-336`): the skills listing is
    /// injected only when the `read` tool is in `active_tool_names`. The model
    /// needs `read` to load a skill's full file on invocation; without it, the
    /// listing would advertise skills the model cannot act on, so pi suppresses
    /// it. The `disable_model_invocation` filter is applied inside
    /// `format_skills_for_system_prompt`.
    fn compose_prompt(
        base: Option<&str>,
        resources: &AgentHarnessResources,
        active_tool_names: &[String],
    ) -> String {
        let all_skills: &[Skill] = resources.skills.as_deref().unwrap_or(&[]);
        let skills: &[Skill] = if active_tool_names.iter().any(|n| n == "read") {
            all_skills
        } else {
            &[]
        };
        compose_system_prompt(base, skills, None, None)
    }

    /// Reserve an entry id. Exposed so a settle observer can name an entry in a
    /// `step_attempt` record *before* committing it, which is what makes the
    /// record's `result_entry_id` checkable at recovery.
    pub(crate) fn next_entry_id(&self) -> String {
        self.session.id_generator().next()
    }

    /// Write a `tool_started` record on this harness's lane.
    ///
    /// This is the record that lets recovery name the tool whose side effect is
    /// unknown after a crash. It is only writable *after* the assistant message
    /// carrying the call is an entry, which is what commit-on-settle buys.
    /// Takes a fully-formed record rather than its fields: the caller already
    /// assembles the parts (assistant entry, ordinal, reserved result id), and a
    /// nine-parameter signature would only split one construction across two
    /// places.
    pub(crate) async fn write_tool_started(&self, record: ToolStartedRecord) -> HarnessResult<()> {
        self.session
            .append_record(LaneRecord::ToolStarted(record))
            .await
            .map_err(|error| {
                self.record_write_failure("a tool execution checkpoint", &error);
                session_to_harness_err(error)
            })?;
        Ok(())
    }

    /// Commit a provisioned entry on this harness's lane.
    pub(crate) async fn append_provisioned(&self, entry: ProvisionedEntry) -> HarnessResult<Entry> {
        // The ACK and queue publication must survive caller drop together.
        self.commit_reserved_prompt(entry).await
    }

    async fn append_provisioned_locked(&self, entry: ProvisionedEntry) -> HarnessResult<Entry> {
        self.session
            .append_entry(entry, &self.lane)
            .await
            .map_err(session_to_harness_err)
    }

    /// Commit the frame-stream binding, assistant entry, and step attempt in
    /// one durable append. The logical order remains binding → entry → step.
    pub(crate) async fn append_bound_provisioned_with_step_attempt(
        &self,
        entry: ProvisionedEntry,
        run_id: &str,
        attempt: u32,
        stream_index: usize,
    ) -> HarnessResult<Entry> {
        let _gate = self.message_writes.lock().await;
        let bind = LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            run_id: run_id.to_string(),
            stream_index,
            op: AssistantFrameOp::CommitStream,
            frame: Some(serde_json::json!({"entryId": entry.id})),
        });
        let step = LaneRecord::StepAttempt(StepAttemptRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            run_id: run_id.to_string(),
            step: StepKind::Assistant,
            attempt,
            result_entry_id: entry.id.clone(),
            compaction_reason: None,
        });
        let (_, committed, _) = self
            .session
            .append_record_entry_and_record(bind, entry, &self.lane, step)
            .await
            .map_err(session_to_harness_err)?;
        Ok(committed)
    }

    /// Record that another attempt is scheduled for `run_id`.
    ///
    /// Written before the backoff wait, so a crash during the wait is
    /// recognisable as "this run was about to retry" rather than looking like an
    /// interrupted run. The reserved id is that attempt's assistant entry: once it
    /// lands, the retry succeeded and nothing is pending.
    async fn write_retry_pending(
        &self,
        run_id: &str,
        attempt: u32,
        result_entry_id: &str,
    ) -> HarnessResult<()> {
        self.session
            .append_record(LaneRecord::RetryPending(RetryPendingRecord {
                base: self.record_base(),
                run_id: run_id.to_string(),
                attempt,
                result_entry_id: result_entry_id.to_string(),
            }))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Persist a `Compaction` entry and return the stamped `Entry`.
    ///
    /// Writes the durable intent records around the commit:
    ///
    /// - `write_deferred` *before* the append, naming the entry this step means
    ///   to commit. A compaction's content is known up front, so its target JSON
    ///   is exact — which is what `validate_exact_provisioned_entry` demands
    ///   (it deep-compares the committed entry against the recorded intent).
    /// - `step_attempt` *after* the append, so `result_entry_id` names the entry
    ///   that actually landed. `validate_attempt_result` accepts an absent entry,
    ///   but a step record whose id never resolves is worse than none: recovery
    ///   would appear to have information it cannot check.
    async fn persist_compaction_entry(
        &self,
        run_id: &str,
        result: &crate::compaction::CompactResult,
        reason: CompactionReason,
    ) -> HarnessResult<Entry> {
        let id = self.session.id_generator().next();
        let details = result
            .details
            .as_ref()
            .map(|d| serde_json::to_value(d).unwrap_or(JsonValue::Null));
        let entry = ProvisionedEntry {
            id: id.clone(),
            kind: ProvisionedKind::Compaction {
                summary: result.summary.clone(),
                retained_tail: result.retained_tail.clone(),
                tokens_before: result.tokens_before,
                details,
                usage: result.usage.clone(),
            },
        };

        self.write_write_deferred(run_id, &entry).await?;
        let _gate = self.message_writes.lock().await;
        let committed = self
            .session
            .append_entry(entry, &self.lane)
            .await
            .map_err(session_to_harness_err)?;
        // A compaction is a step series of its own, so its first attempt is 1
        // (the reducer resets the series when `step` changes).
        self.write_step_attempt(run_id, StepKind::Compaction, 1, &id, Some(reason))
            .await?;
        Ok(committed)
    }

    /// Record a `write_deferred`: an entry whose content is already known but
    /// which has not been committed yet.
    ///
    /// `target` is the entry's *provisioned* JSON (its flat serialization minus
    /// the storage-assigned `seq`/`parentId`/`timestamp`), built through the same
    /// helper the reducer compares against, so the recorded intent and the
    /// eventual commit cannot drift.
    async fn write_write_deferred(
        &self,
        run_id: &str,
        entry: &ProvisionedEntry,
    ) -> HarnessResult<()> {
        let placeholder = crate::session::types::provisioned_into_entry(entry.clone(), 0, None, 0);
        let target = crate::session::reducer::entry_provisioned_json(&placeholder);
        self.session
            .append_record(LaneRecord::WriteDeferred(WriteDeferredRecord {
                base: RecordBase {
                    id: self.session.id_generator().next(),
                    seq: 0,
                    lane: self.lane.clone(),
                    timestamp: 0,
                },
                run_id: run_id.to_string(),
                target,
            }))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Record a `step_attempt`: one durable step of a run, naming the entry that
    /// will (or did) hold its result.
    async fn write_step_attempt(
        &self,
        run_id: &str,
        step: StepKind,
        attempt: u32,
        result_entry_id: &str,
        compaction_reason: Option<CompactionReason>,
    ) -> HarnessResult<()> {
        self.session
            .append_record(LaneRecord::StepAttempt(StepAttemptRecord {
                base: RecordBase {
                    id: self.session.id_generator().next(),
                    seq: 0,
                    lane: self.lane.clone(),
                    timestamp: 0,
                },
                run_id: run_id.to_string(),
                step,
                attempt,
                result_entry_id: result_entry_id.to_string(),
                compaction_reason,
            }))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    fn operation_started_record(
        &self,
        run_id: &str,
        source_leaf_id: Option<String>,
        intent: OperationIntent,
    ) -> LaneRecord {
        LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: run_id.to_string(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            source_leaf_id,
            intent,
        })
    }

    /// Write an `operation_started` record. Returns the stamped record (its
    /// `base.id` is the run_id the caller generated; `seq`/`lane`/`timestamp`
    /// are overwritten by storage).
    async fn write_operation_started(
        &self,
        run_id: &str,
        source_leaf_id: Option<String>,
        intent: OperationIntent,
    ) -> HarnessResult<OperationStartedRecord> {
        let stamped = self
            .session
            .append_record(self.operation_started_record(run_id, source_leaf_id, intent))
            .await
            .map_err(session_to_harness_err)?;
        match stamped {
            LaneRecord::OperationStarted(s) => Ok(s),
            _ => unreachable!("append_record(OperationStarted) stamps into OperationStarted"),
        }
    }

    /// Commit `operation_started` followed by the first prompt entry in one
    /// durable append when the backend supports it. The record remains first.
    async fn append_operation_started_and_entry(
        &self,
        run_id: &str,
        source_leaf_id: Option<String>,
        intent: OperationIntent,
        entry: ProvisionedEntry,
    ) -> HarnessResult<Entry> {
        self.commit_initial_prompt(
            self.operation_started_record(run_id, source_leaf_id, intent),
            entry,
        )
        .await
    }

    /// Write an `operation_finished` record.
    async fn write_operation_finished(
        &self,
        run_id: &str,
        outcome: OperationOutcome,
        error: Option<OperationError>,
    ) -> HarnessResult<()> {
        let _gate = self.message_writes.lock().await;
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(active) = inner
                .active_run
                .as_mut()
                .filter(|active| active.run_id == run_id)
            {
                // Serialize this transition with legacy queue enqueues too:
                // their records must not name an operation that just closed.
                active.finishing = true;
            }
        }
        let record = LaneRecord::OperationFinished(OperationFinishedRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            run_id: run_id.to_string(),
            outcome,
            error,
            result: None,
        });
        self.session
            .append_record(record)
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Persist each `AgentMessage` produced by the loop. The prompts were
    /// already persisted before the run, and `run_agent_loop` is called with
    /// an empty prompts vec (see `run_core`), so `new_messages` contains ONLY
    /// loop-produced messages — persist every one of them. Returns
    /// `(leaf_id, final_entry_id)`.
    async fn persist_new_messages(
        &self,
        new_messages: &[AgentMessage],
        start_index: usize,
        settle: &crate::settle::SettleState,
    ) -> HarnessResult<(Option<String>, Option<String>)> {
        // `start_index` is where `new_messages[0]` sits in the loop's full message
        // sequence, so a settle-committed index lines up with slice offsets.
        // `committed` holds the indices the settle observer already persisted.
        let mut leaf_id = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;
        let committed = settle.committed_indices();
        let mut final_entry_id: Option<String> = leaf_id.clone();
        for (offset, msg) in new_messages.iter().enumerate() {
            // A message the settle observer already committed is durable; writing
            // it again would duplicate it in the branch.
            if committed.contains(&(start_index + offset)) {
                final_entry_id = settle.entry_at(start_index + offset).or(final_entry_id);
                continue;
            }
            let id = settle.reserve_entry_id(start_index + offset, self);
            settle
                .prepare_commit(start_index + offset, &id, self)
                .await?;
            self.append_provisioned(ProvisionedEntry {
                id: id.clone(),
                kind: ProvisionedKind::Message {
                    message: msg.clone(),
                    terminate: None,
                },
            })
            .await?;
            settle.record_commit(start_index + offset, &id);
            leaf_id = Some(id.clone());
            final_entry_id = Some(id);
        }
        Ok((leaf_id, final_entry_id))
    }

    /// Derive the harness `RunOutcome` from the terminal assistant message.
    fn derive_outcome(
        new_messages: &[AgentMessage],
        leaf_id: Option<String>,
        final_entry_id: Option<String>,
    ) -> HarnessRunOutcome {
        let leaf = leaf_id.unwrap_or_default();
        let final_id = final_entry_id.unwrap_or_default();
        // Find the last assistant message.
        let last_assistant = new_messages.iter().rev().find_map(|m| match m {
            AgentMessage::Assistant(a) => Some(a.as_ref()),
            _ => None,
        });
        match last_assistant {
            None => HarnessRunOutcome::Failed {
                leaf_id: leaf,
                error: OperationError {
                    code: "no_assistant".into(),
                    message: "Run ended without producing an assistant message".into(),
                },
                final_entry_id: if final_id.is_empty() {
                    None
                } else {
                    Some(final_id)
                },
                final_message: None,
            },
            Some(am) => match am.stop_reason {
                StopReason::Error => HarnessRunOutcome::Failed {
                    leaf_id: leaf,
                    error: OperationError {
                        code: "error".into(),
                        message: am.error_message.clone().unwrap_or_default(),
                    },
                    final_entry_id: if final_id.is_empty() {
                        None
                    } else {
                        Some(final_id)
                    },
                    final_message: Some(am.clone()),
                },
                StopReason::Aborted => HarnessRunOutcome::Aborted {
                    leaf_id: leaf,
                    final_entry_id: final_id,
                    final_message: am.clone(),
                },
                StopReason::Deferred => match &am.deferred {
                    Some(handle) => HarnessRunOutcome::Suspended {
                        leaf_id: leaf,
                        final_entry_id: final_id,
                        deferred: handle.clone(),
                    },
                    None => HarnessRunOutcome::Failed {
                        leaf_id: leaf,
                        error: OperationError {
                            code: "deferred_no_handle".into(),
                            message: "Assistant returned Deferred stop reason without a handle"
                                .into(),
                        },
                        final_entry_id: if final_id.is_empty() {
                            None
                        } else {
                            Some(final_id)
                        },
                        final_message: Some(am.clone()),
                    },
                },
                _ => HarnessRunOutcome::Completed {
                    leaf_id: leaf,
                    final_entry_id: final_id,
                    final_message: am.clone(),
                },
            },
        }
    }

    /// The core run loop shared by all prompt overloads + skill + template.
    async fn run_core(&self, prompts: Vec<AgentMessage>) -> HarnessResult<RunResult> {
        // Image preparation is part of the admitted run: RPC cancellation must
        // find an active signal and await this future, not detach its blocking
        // decoder by dropping a pre-admission prompt. Bridge the lease through
        // preparation; the admitted loop installs its own run-ID-checked lease.
        let admission = self.acquire_run(OperationKind::Run).await?;
        let _preparation_lease = ActiveRunLease {
            harness: self,
            run_id: admission.0.clone(),
        };
        let mut normalized = Vec::with_capacity(prompts.len());
        for prompt in prompts {
            if admission.1.is_cancelled() {
                return Err(HarnessError::agent(
                    "Operation aborted during image preparation",
                ));
            }
            let prompt = self.expand_skill_input(prompt, Some(&admission.1)).await;
            normalized.push(normalize_user_images(prompt).await?);
        }
        // Await an already-started decoder even on abort, but never persist or
        // call the model after preparation observes cancellation.
        if admission.1.is_cancelled() {
            return Err(HarnessError::agent(
                "Operation aborted during image preparation",
            ));
        }
        self.run_core_with_admission(normalized, false, admission, None)
            .await
    }

    /// Continue a run from the assistant message at the branch tip.
    ///
    /// The deferred case: that message came from a poll, so its tool calls have
    /// not run. Entering the loop *from* it executes them before the next
    /// provider request instead of sending unanswered tool calls.
    async fn run_core_from_assistant(&self, parked_run_id: &str) -> HarnessResult<RunResult> {
        let admission = {
            let _gate = self.message_writes.lock().await;
            // Transfer admission atomically: another prompt must not get
            // between the polled assistant and its unanswered tool calls.
            self.release_run(parked_run_id);
            self.acquire_run_locked(OperationKind::Run)?
        };
        self.run_core_with_admission(Vec::new(), true, admission, None)
            .await
    }

    async fn run_core_with_admission(
        &self,
        prompts: Vec<AgentMessage>,
        from_assistant: bool,
        admission: (String, CancellationToken, Arc<tokio::sync::Notify>),
        resume: Option<PendingResume>,
    ) -> HarnessResult<RunResult> {
        let scope = self.inner.lock().unwrap().execution_scope.clone();
        let run = self.run_admitted_inner(prompts, from_assistant, admission, resume);
        match scope {
            Some(scope) => scope.run(run).await,
            None => run.await,
        }
    }

    async fn run_admitted_inner(
        &self,
        prompts: Vec<AgentMessage>,
        from_assistant: bool,
        admission: (String, CancellationToken, Arc<tokio::sync::Notify>),
        resume: Option<PendingResume>,
    ) -> HarnessResult<RunResult> {
        let (run_id, signal, _idle) = admission;
        let _run_lease = ActiveRunLease {
            harness: self,
            run_id: run_id.clone(),
        };

        // `nextRun` messages are consumed when a new run starts, before the
        // caller's prompt. This matches pi's ordering and keeps them durable
        // through the normal prompt persistence path below.
        // Resuming an interrupted/deferred run is not an explicit next turn.
        let reservation = if prompts.is_empty() || from_assistant {
            None
        } else {
            Some(self.reserve_next_run().await)
        };
        let queued = reservation
            .as_ref()
            .map(|reservation| reservation.items.clone())
            .unwrap_or_default();
        let queued_ids: Vec<String> = queued.iter().map(|item| item.entry_id.clone()).collect();
        let prompts: Vec<AgentMessage> = queued
            .into_iter()
            .map(|item| item.message)
            .chain(prompts)
            .collect();
        // Recovery runs before the source leaf is snapshotted, so the salvaged
        // messages are part of the base history this run builds on.
        let recovery_tools = self.recovery_tools();
        // `true`: a run starting here retries through its own retry loop, so a
        // leftover failed attempt is settled the ordinary way.
        Self::finish_interrupted_operation(
            &self.session,
            &self.lane,
            &recovery_tools,
            &signal,
            true,
            Some(&self.write_failures),
        )
        .await?;

        // Recovery resolves interrupted calls before accepted context is added.
        let gate = self.message_writes.lock().await;
        if let Some(active) = self.inner.lock().unwrap().active_run.as_mut() {
            active.recovering = false;
        }
        let restored = if from_assistant {
            Vec::new()
        } else {
            self.flush_context_messages_locked().await?
        };
        drop(gate);
        self.emit_appended_messages(restored).await;

        // Snapshot the source leaf (before persisting prompts) for the
        // operation_started record.
        let source_leaf = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;

        // Emit RunStart.
        self.bus.emit(&HarnessEvent::RunStart(RunStartEvent {
            lane: self.lane.clone(),
            run_id: run_id.clone(),
        }));

        // Write operation_started.
        // A resumed run records the chain it belongs to. `resume_data` was an
        // unused placeholder until now; the attempt budget has to be durable,
        // because the crash that makes it matter also destroys every in-memory
        // counter that could have held it.
        let resume_data = resume.map(|pending| {
            let mut data = std::collections::BTreeMap::new();
            data.insert(
                "resumedFrom".to_string(),
                JsonValue::String(pending.chain_origin_run_id),
            );
            data.insert(
                "resumeAttempt".to_string(),
                JsonValue::from(pending.attempt + 1),
            );
            data
        });
        let intent = OperationIntent::Run {
            original_prompt: prompts.clone(),
            initial_messages: Vec::new(),
            system_prompt_override: None,
            resume_data,
        };
        let mut prompt_ids = Vec::with_capacity(prompts.len());
        let prompt_start = if let Some(first) = prompts.first() {
            let first_entry_id = queued_ids
                .first()
                .cloned()
                .unwrap_or_else(|| self.next_entry_id());
            self.append_operation_started_and_entry(
                &run_id,
                source_leaf.clone(),
                intent,
                ProvisionedEntry {
                    id: first_entry_id.clone(),
                    kind: ProvisionedKind::Message {
                        message: first.clone(),
                        terminate: None,
                    },
                },
            )
            .await
            .map_err(|error| {
                // The first user prompt rides the operation_started append, so
                // this is the normal prompt path's refused-write point — the
                // same "on screen but not saved" case `append_message` covers.
                // Without a recovery copy here the warning's /export advice is
                // hollow for the one write every conversation starts with.
                self.record_write_failure_message("a message you sent", &error.to_string());
                self.keep_unsaved_message("a message you sent", first.clone());
                error
            })?;
            prompt_ids.push(first_entry_id);
            1
        } else {
            self.write_operation_started(&run_id, source_leaf.clone(), intent)
                .await?;
            0
        };

        // Persist the remaining prompts (the user's input) BEFORE the run so
        // they're durable. The first prompt, when present, was committed with
        // operation_started above.
        for (index, msg) in prompts.iter().enumerate().skip(prompt_start) {
            let persisted = if let Some(entry_id) = queued_ids.get(index) {
                self.commit_reserved_prompt(ProvisionedEntry {
                    id: entry_id.clone(),
                    kind: ProvisionedKind::Message {
                        message: msg.clone(),
                        terminate: None,
                    },
                })
                .await
                .map(|entry| entry.base().id.clone())
            } else {
                self.view()
                    .append_message(msg.clone())
                    .await
                    .map_err(session_to_harness_err)
            };
            if let Ok(id) = &persisted {
                prompt_ids.push(id.clone());
            }
            if let Err(error) = persisted {
                // Same recovery contract as the first prompt: the text is on
                // screen, the write was refused, and /export must be able to
                // keep it. Each refused prompt gets its own copy so the export
                // shows the whole unsent turn, not just its first line.
                self.record_write_failure_message("a message you sent", &error.to_string());
                self.keep_unsaved_message("a message you sent", msg.clone());
                return Err(error);
            }
        }

        self.emit_appended_messages(
            prompt_ids
                .into_iter()
                .zip(prompts.iter().cloned())
                .take(queued_ids.len())
                .collect(),
        )
        .await;

        let snap = self.snapshot_config()?;
        let build_opts = Self::build_opts(&snap.entry_projectors, &snap.entry_transforms);
        if snap.models.is_empty() {
            let mut outcome = HarnessRunOutcome::Failed {
                leaf_id: String::new(),
                error: OperationError {
                    code: "no_provider".into(),
                    message: "No models/providers configured".into(),
                },
                final_entry_id: None,
                final_message: None,
            };
            self.finish_run_result(&run_id, &mut outcome).await?;
            return Ok(RunResult { run_id, outcome });
        }

        // Pre-run compaction evaluation. Build the same context that the run
        // will use, but retain it when no compaction is needed. The old path
        // loaded and projected the complete branch here and then rebuilt it
        // again immediately below.
        let mut prebuilt_context: Option<crate::session::context::SessionContext> = None;
        if snap.compaction.enabled && snap.model.context_window > 0 {
            let ctx = if build_opts.entry_transforms.is_empty()
                && build_opts.entry_projectors.is_empty()
            {
                self.branch_context_default().await?
            } else {
                let path = self.branch_path_oldest_first().await?;
                build_session_context(&path, &build_opts)
            };
            let ctx_tokens = estimate_context_tokens(&ctx.messages).tokens;
            let window = snap.model.context_window as i64;
            if !should_compact(ctx_tokens, window, &snap.compaction) {
                if build_opts.entry_transforms.is_empty() && build_opts.entry_projectors.is_empty()
                {
                    prebuilt_context = Some(ctx);
                }
            } else {
                // Only materialize the full entry path when the cheap context
                // check says compaction may actually be necessary.
                let path = self.branch_path_oldest_first().await?;
                let mut compaction_applied = false;
                if let Ok(Some(prep)) = prepare_compaction(&path, snap.compaction) {
                    let compaction_provider = Self::resolve_provider(&snap.models, &snap.model)
                        .or_else(|_| {
                            // Fall back to first provider if the model's
                            // provider isn't registered but others are.
                            snap.models
                                .first()
                                .cloned()
                                .ok_or_else(|| HarnessError::agent("No provider for compaction"))
                        });
                    if let Ok(provider) = compaction_provider {
                        let llm_opts = CompactionLlmOptions {
                            provider,
                            model: snap.model.clone(),
                            api_key: None,
                            signal: signal.clone(),
                            thinking_level: Some(snap.thinking_level),
                            retry: if snap.retry.enabled {
                                Some(snap.retry.clone())
                            } else {
                                None
                            },
                            custom_instructions: None,
                            stream_options: snap.stream_options.clone(),
                        };
                        match {
                            let _progress = crate::events::CompactionProgressGuard::new(
                                &self.bus, &self.lane, false,
                            );
                            compact(&prep, &llm_opts).await
                        } {
                            Ok(result) => {
                                let _ = self
                                    .persist_compaction_entry(
                                        &run_id,
                                        &result,
                                        // The pre-run check fires when the context
                                        // crosses the configured threshold, i.e.
                                        // proactively, not in reaction to a provider
                                        // overflow error.
                                        CompactionReason::Threshold,
                                    )
                                    .await?;
                                compaction_applied = true;
                            }
                            Err(e) => {
                                let mut outcome = HarnessRunOutcome::Failed {
                                    leaf_id: String::new(),
                                    error: OperationError {
                                        code: "compaction_failed".into(),
                                        message: e.message.clone(),
                                    },
                                    final_entry_id: None,
                                    final_message: None,
                                };
                                self.finish_run_result(&run_id, &mut outcome).await?;
                                return Ok(RunResult { run_id, outcome });
                            }
                        }
                    }
                }
                if !compaction_applied
                    && build_opts.entry_transforms.is_empty()
                    && build_opts.entry_projectors.is_empty()
                {
                    prebuilt_context = Some(ctx);
                }
            }
        }

        let ctx = if let Some(ctx) = prebuilt_context {
            ctx
        } else if build_opts.entry_transforms.is_empty() && build_opts.entry_projectors.is_empty() {
            self.branch_context_default().await?
        } else {
            let path = self.branch_path_oldest_first().await?;
            build_session_context_owned(path, &build_opts)
        };
        let system_prompt = Self::compose_prompt(
            snap.system_prompt.as_deref(),
            &snap.resources,
            &snap.active_tool_names,
        );
        let tools = Self::active_tools(&snap.tools, &snap.active_tool_names);

        let mut agent_context = AgentContext {
            system_prompt,
            messages: ctx.messages,
            tools,
        };

        // Build the config.
        let custom_converter = snap.to_provider_messages.is_some();
        let convert = Self::build_convert_to_llm(snap.to_provider_messages);
        let convert_to_llm_incremental = if custom_converter {
            None
        } else {
            Some(Arc::new(harness_convert_to_llm_incremental) as rpi_agent::IncrementalConvertToLlm)
        };
        let stream_fn = Self::build_stream_fn(
            snap.models.clone(),
            snap.provider_hooks.clone(),
            snap.stream_options.clone(),
        )?;
        let post_compaction_cut = Arc::new(AtomicUsize::new(0));
        // Run-level turn ceiling shared with the post-loop notice below. The
        // loop's only other exit is "the model stopped asking for tools", which
        // nothing bounds — one observed session ran 112 turns / 867s in a single
        // run (`docs/debugging/llm-repetition-forensics.md` §二). Upstream has no such
        // ceiling either, so this is **off unless** `RPI_MAX_TURNS_PER_RUN=<n>`
        // asks for it; when off the hook stays `None` and the loop is unchanged.
        let run_budget = Arc::new(Mutex::new(match self.max_turns {
            Some(limit) => crate::run_budget::RunBudget::new(limit),
            None => crate::run_budget::RunBudget::from_env(),
        }));
        // `should_stop_after_turn` is `Option<fn>` — only one hook fits — so the
        // turn budget and the user's `StopRound` request share it. Either one
        // being true ends the run at the turn boundary.
        //
        let stop_round_flag = Arc::clone(&self.stop_round);
        let budget_enabled = run_budget.lock().unwrap().is_enabled();
        let should_stop_after_turn: Option<rpi_agent::ShouldStopAfterTurn> = if budget_enabled {
            let budget = Arc::clone(&run_budget);
            Some(Arc::new(
                move |_ctx: ShouldStopAfterTurnContext<'_>| -> BoxFuture<'static, bool> {
                    let budget = Arc::clone(&budget);
                    let flag = Arc::clone(&stop_round_flag);
                    Box::pin(async move {
                        // The user's "stop after this round" wins and is
                        // **one-shot**: consume the flag, or the next prompt
                        // would stop immediately too.
                        let requested = flag.swap(false, std::sync::atomic::Ordering::SeqCst);
                        requested || budget.lock().unwrap().observe_turn().is_some()
                    })
                },
            ))
        } else {
            // No turn ceiling, but the hook must still exist — without it
            // `StopRound` has nowhere to take effect.
            Some(Arc::new(
                move |_ctx: ShouldStopAfterTurnContext<'_>| -> BoxFuture<'static, bool> {
                    let flag = Arc::clone(&stop_round_flag);
                    Box::pin(async move { flag.swap(false, std::sync::atomic::Ordering::SeqCst) })
                },
            ))
        };

        let retry_policy = snap.retry.clone();
        let mut retry_attempt = 0u32;
        // Commit-on-settle: persist an assistant message carrying tool calls as
        // soon as it is final, so the tools it calls can be recorded before they
        // run (`tool_started` requires the assistant entry to exist).
        let settle_state = Arc::new(crate::settle::SettleState::new(
            run_id.clone(),
            retry_policy.enabled,
            retry_policy.max_retries,
            snap.tools
                .iter()
                .map(|tool| {
                    // `HarnessTool` carries the harness-level policy; a record
                    // carries the persisted one. They are separate types with the
                    // same two cases, so map explicitly rather than relying on
                    // them staying in sync.
                    let replay = match tool.replay {
                        crate::types::ToolReplay::Never => ToolReplay::Never,
                        crate::types::ToolReplay::Safe => ToolReplay::Safe,
                    };
                    (tool.tool.schema().name.clone(), replay)
                })
                .collect(),
        ));
        // A deferred poll's assistant is already history, but this fresh run
        // has not observed its MessageEnd. Seed call registrations from the
        // durable source entry, never by searching for matching text/content
        // and never by re-appending it or inventing an assistant identity.
        if from_assistant {
            let entry_id = source_leaf.as_deref().ok_or_else(|| {
                HarnessError::agent("resumed assistant has no durable source entry")
            })?;
            let Some(Entry::Message(entry)) = self.get_entry(entry_id).await? else {
                return Err(HarnessError::agent(
                    "resumed assistant source entry is missing",
                ));
            };
            let AgentMessage::Assistant(assistant) = &entry.message else {
                return Err(HarnessError::agent(
                    "resumed source entry is not an assistant",
                ));
            };
            let Some(AgentMessage::Assistant(context_assistant)) = agent_context.messages.last()
            else {
                return Err(HarnessError::agent(
                    "resumed context does not end in an assistant",
                ));
            };
            // Tools execute in content-block order. A context transform must
            // not silently reassign a durable call to a different slot.
            let call_slots = |message: &AssistantMessage| {
                message
                    .content
                    .iter()
                    .enumerate()
                    .filter_map(|(index, block)| {
                        if let Content::ToolCall(call) = block {
                            Some((index, call.id.clone(), call.name.clone()))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            };
            if call_slots(assistant) != call_slots(context_assistant) {
                return Err(HarnessError::agent(
                    "resumed tool call slots differ from durable assistant",
                ));
            }
            self.write_step_attempt(&run_id, StepKind::Assistant, 1, entry_id, None)
                .await?;
            settle_state.record_tool_calls(entry_id, assistant);
        }
        // Shared fail-closed exit for execution checkpoints and context writes.
        let context_write_error = Arc::new(Mutex::new(None));
        agent_context.tools = agent_context
            .tools
            .into_iter()
            .map(|tool| {
                Arc::new(crate::tool_execution::ToolExecutionBarrier {
                    tool,
                    harness: self.clone(),
                    settle: Arc::clone(&settle_state),
                    write_error: Arc::clone(&context_write_error),
                    signal: signal.clone(),
                }) as Arc<dyn rpi_agent::AgentTool>
            })
            .collect();
        let mut config = AgentLoopConfig {
            model: snap.model.clone(),
            convert_to_llm: convert,
            convert_to_llm_incremental,
            transform_context: snap.transform_context.clone(),
            get_api_key: None,
            should_stop_after_turn,
            prepare_next_turn: None,
            after_tool_results: {
                let harness = self.clone();
                let enabled = snap.compaction.enabled;
                let settings = snap.compaction;
                let stream_options = snap.stream_options.clone();
                let model = snap.model.clone();
                let provider = snap
                    .models
                    .iter()
                    .find(|p| p.id() == model.provider)
                    .cloned();
                let compaction_signal = signal.clone();
                let compaction_run_id = run_id.clone();
                if enabled && model.context_window > 0 && provider.is_some() {
                    let cut_for_hook = Arc::clone(&post_compaction_cut);
                    Some(Arc::new(move |ctx: ShouldStopAfterTurnContext<'_>| -> BoxFuture<'static, Option<rpi_agent::AgentLoopTurnUpdate>> {
                        let harness = harness.clone();
                        let model = model.clone();
                        let provider = provider.clone().expect("checked above");
                        let cut = Arc::clone(&cut_for_hook);
                        let compaction_signal = compaction_signal.clone();
                        let stream_options = stream_options.clone();
                        let compaction_run_id = compaction_run_id.clone();
                        let new_messages_len = ctx.new_messages.len();
                        // Estimate from the borrowed context. Cloning the entire
                        // history here ran on every turn just to answer "is
                        // compaction needed?"; only snapshot it when it is.
                        let tokens = estimate_context_tokens(&ctx.context.messages).tokens;
                        if !should_compact(tokens, model.context_window as i64, &settings) {
                            return Box::pin(async { None });
                        }
                        let context = ctx.context.clone();
                        Box::pin(async move {
                            let entries: Vec<Entry> = context.messages.iter().enumerate().map(|(i, message)| Entry::Message(crate::session::types::MessageEntry {
                                base: crate::session::types::EntryBase { entry_type: "message".into(), id: format!("runtime-{i}"), seq: i as u64 + 1, parent_id: None, timestamp: i as i64 },
                                message: message.clone(), terminate: None,
                            })).collect();
                            let prep = match prepare_compaction(&entries, settings) { Ok(Some(p)) => p, _ => return None };
                            let opts = CompactionLlmOptions { provider, model: model.clone(), api_key: None, signal: compaction_signal, thinking_level: None, retry: None, custom_instructions: None, stream_options };
                            let result = match { let _progress = crate::events::CompactionProgressGuard::new(&harness.bus, &harness.lane, false); compact(&prep, &opts).await } { Ok(r) => r, Err(_) => return None };
                            // Mid-loop compaction fires because the turn pushed the
                            // context over the threshold.
                            if harness.persist_compaction_entry(&compaction_run_id, &result, CompactionReason::Threshold).await.is_err() { return None; }
                            cut.store(new_messages_len, Ordering::Release);
                            let mut messages = Vec::with_capacity(result.retained_tail.len() + 1);
                            messages.push(create_compaction_summary_message(&result.summary, result.tokens_before, 0));
                            messages.extend(result.retained_tail);
                            Some(rpi_agent::AgentLoopTurnUpdate { context: Some(AgentContext { system_prompt: context.system_prompt.clone(), messages, tools: context.tools.clone() }), model: None, thinking_level: None })
                        })
                    }) as AfterToolResults)
                } else {
                    None
                }
            },
            get_steering_messages: Some({
                let queue = Arc::clone(&snap.steering_queue);
                let harness = self.clone();
                let settle = Arc::clone(&settle_state);
                Arc::new(move || {
                    let harness = harness.clone();
                    let queue = Arc::clone(&queue);
                    let settle = Arc::clone(&settle);
                    Box::pin(async move { harness.drain_injected(queue, &settle).await })
                })
            }),
            get_follow_up_messages: Some({
                let queue = Arc::clone(&snap.follow_up_queue);
                let harness = self.clone();
                let settle = Arc::clone(&settle_state);
                Arc::new(move || {
                    let harness = harness.clone();
                    let queue = Arc::clone(&queue);
                    let settle = Arc::clone(&settle);
                    Box::pin(async move { harness.drain_injected(queue, &settle).await })
                })
            }),
            // Read-only queue depth (**does not drain**).
            //
            // The two drain hooks above cannot answer "how many are left?" —
            // calling them would consume the messages. This reads the harness's
            // own queues, the same data the drains read, so the "⇥ N queued"
            // indicator cannot drift from reality.
            pending_message_count: Some({
                let steer = Arc::clone(&snap.steering_queue);
                let follow = Arc::clone(&snap.follow_up_queue);
                // Queues are per lane, so ask for **this** harness's lane rather
                // than hardcoding "main": lanes made by `AgentHarness::lane(name)`
                // run through the same `run_admitted_inner`, and hardcoding would
                // count their messages as main's (or always report 0).
                let lane = self.lane.clone();
                Arc::new(move || {
                    steer.lock().map(|q| q.len_for_lane(&lane)).unwrap_or(0)
                        + follow.lock().map(|q| q.len_for_lane(&lane)).unwrap_or(0)
                })
            }),
            before_tool_call: snap.before_tool_call.clone(),
            after_tool_call: snap.after_tool_call.clone(),
            tool_execution: snap.tool_execution.to_agent_mode(),
            thinking_level: snap.thinking_level,
            api_key: None,
            timeout: snap.stream_options.timeout,
            stream_idle_timeout: snap.stream_options.stream_idle_timeout,
            // Pi's `retry.maxRetries` is an assistant-level retry budget. The
            // provider/SDK retry budget is intentionally independent and
            // defaults to zero, so transient failures are retried by the
            // harness exactly once per agent attempt.
            max_retries: None,
            max_retry_delay: Some(std::time::Duration::from_millis(
                snap.retry.max_agent_delay_ms,
            )),
            cache_retention: snap.stream_options.cache_retention.unwrap_or_default(),
            // Providers key prompt caching and session affinity on it (TS
            // `sessionId: this.session.id`); without it every turn may miss.
            session_id: Some(self.session.storage().metadata().id),
            signal: signal.clone(),
        };

        // Deliver accepted context before compaction and again after turn-end
        // handlers, including turns with no tools. Neither hook drives a turn.
        config.after_tool_results = Some(self.context_boundary_hook(
            config.after_tool_results.take(),
            Arc::clone(&settle_state),
            signal.clone(),
            Arc::clone(&context_write_error),
        ));
        config.prepare_next_turn = Some(self.context_boundary_hook(
            None,
            Arc::clone(&settle_state),
            signal.clone(),
            Arc::clone(&context_write_error),
        ));

        // Emitter: use the caller's override if one was supplied (e.g. the TUI
        // installs a BroadcastEmitter so it can render AgentEvents live);
        // otherwise fall back to a collector that discards events (the harness
        // still surfaces RunStart/RunEnd via its own bus).
        let base_emitter: Arc<dyn AgentEmitter> = snap
            .agent_emitter
            .clone()
            .unwrap_or_else(|| Arc::new(rpi_agent::CollectorEmitter::default()));
        // Durable progress: every streamed assistant frame is appended to the
        // session as it arrives, so a crash mid-run leaves a committed prefix
        // that `salvage_run_frames` can replay instead of losing the whole run.
        // Mirrors upstream's `openFrameProgress`. See
        // `docs/debugging/llm-repetition-forensics.md` §十一.
        let frame_recorder = Arc::new(crate::frame_progress::FrameRecordingEmitter::new(
            base_emitter,
            self.session.clone(),
            self.lane.clone(),
            run_id.clone(),
        ));
        settle_state.set_frame_recorder(Arc::clone(&frame_recorder));
        let emitter: Arc<dyn AgentEmitter> = frame_recorder.clone();

        // Drive the loop. We pass an EMPTY prompts vec to `run_agent_loop`
        // (NOT `prompts`): the prompts were already persisted to the session
        // and are present in `agent_context.messages` via the branch-path
        // build above. Passing them again would extend `current_context` a
        // second time and double-count them in the provider context.
        //
        // Consequence for persistence: `run_agent_loop` seeds `new_messages`
        // from `prompts.clone()` (empty here), so the returned `new_messages`
        // contains ONLY the loop-produced messages (assistant / tool-result /
        // assistant-final) — NO prompt prefix. `persist_new_messages` must
        // therefore persist ALL of them (skip 0). The old `skip(prompts_len)`
        // was a leftover from a design that passed `prompts` in; it wrongly
        // dropped the first real assistant message.
        let emitter: Arc<dyn AgentEmitter> = Arc::new(crate::settle::SettlingEmitter::new(
            Arc::clone(&emitter),
            Arc::clone(&frame_recorder),
            Arc::clone(&settle_state),
            self.clone(),
            signal.clone(),
        ));
        let result = loop {
            // An attempt owns its history. A retry reloads the durable branch
            // below, so retaining a deep copy of these messages is unnecessary.
            let attempt_context = AgentContext {
                system_prompt: agent_context.system_prompt.clone(),
                messages: std::mem::take(&mut agent_context.messages),
                tools: agent_context.tools.clone(),
            };
            // Entering from an assistant runs that message's tools as its very
            // first action, which is exactly why it is never retried (see below).
            let attempt_result = if from_assistant {
                run_agent_loop_from_assistant(
                    attempt_context,
                    config.clone(),
                    Arc::clone(&emitter),
                    Arc::clone(&stream_fn),
                )
                .await
            } else {
                run_agent_loop(
                    Vec::new(),
                    attempt_context,
                    config.clone(),
                    Arc::clone(&emitter),
                    Arc::clone(&stream_fn),
                )
                .await
            };

            if let Some(error) = settle_state.identity_error() {
                return Err(HarnessError::io(error));
            }
            let retry_error = attempt_result
                .as_ref()
                .ok()
                .and_then(|messages| {
                    messages.iter().rev().find_map(|message| match message {
                        AgentMessage::Assistant(assistant) => Some(assistant.as_ref()),
                        _ => None,
                    })
                })
                .filter(|assistant| {
                    assistant.stop_reason == StopReason::Error
                        && crate::compaction::is_retryable_assistant_error(assistant)
                })
                .map(|assistant| {
                    assistant
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Provider request failed.".to_string())
                });
            if !retry_policy.enabled
                || retry_attempt >= retry_policy.max_retries
                || retry_error.is_none()
                // A from-assistant attempt already ran the polled message's tools,
                // so retrying would run them a second time. One attempt only.
                || from_assistant
            {
                break attempt_result;
            }

            retry_attempt += 1;
            // Record the retry before waiting, so a crash during the wait is
            // recognised as "this run was about to retry" rather than looking like
            // an interrupted run. The reserved id is that attempt's assistant
            // entry: if it ever lands, the retry succeeded and nothing is pending.
            let retry_result_id = self.next_entry_id();
            if let Err(error) = self
                .write_retry_pending(&run_id, retry_attempt + 1, &retry_result_id)
                .await
            {
                tracing::warn!(%error, "could not record a scheduled retry");
            }
            // Discard only this failed generation, not every stream in the run.
            // Once the next generation starts, recovery must retain its progress.
            if let Some(stream_index) = attempt_result.as_ref().ok().and_then(|messages| {
                messages
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, message)| {
                        matches!(message, AgentMessage::Assistant(_))
                            .then(|| settle_state.stream_at(index))
                            .flatten()
                    })
            }) {
                if let Err(error) = frame_recorder.clear_stream(stream_index).await {
                    tracing::warn!(%error, "could not retire the failed retry generation");
                }
            }
            let delay_ms = retry_policy
                .base_delay_ms
                .saturating_mul(1u64 << retry_attempt.saturating_sub(1))
                .min(retry_policy.max_agent_delay_ms);
            emitter
                .emit(AgentEvent::RetryScheduled {
                    attempt: retry_attempt,
                    max_retries: retry_policy.max_retries,
                    delay_ms,
                    error: retry_error.expect("retry eligibility requires an error"),
                })
                .await;
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                _ = signal.cancelled() => break attempt_result,
            }
            if signal.is_cancelled() {
                break attempt_result;
            }
            // A `StopRound` requested while the scheduler announced the retry or
            // during its backoff wait lands exactly on this turn boundary. It is
            // one-shot, so consume it here too: without this the next provider
            // attempt runs anyway and the request is only honored one turn late.
            if self
                .stop_round
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                break attempt_result;
            }
            // Only discard the old attempt's indices once another invocation
            // will actually run. Compaction cuts use the same per-attempt
            // new_messages indices and must restart with them.
            settle_state.set_attempt(retry_attempt);
            settle_state.reserve_retry_result(retry_result_id);
            post_compaction_cut.store(0, Ordering::Release);
            // Prior tools and delivered context are durable. Rebuild after the
            // wait so any writes accepted during it are included as well.
            agent_context.messages = if build_opts.entry_transforms.is_empty()
                && build_opts.entry_projectors.is_empty()
            {
                self.branch_context_default().await?.messages
            } else {
                let path = self.branch_path_oldest_first().await?;
                build_session_context_owned(path, &build_opts).messages
            };
        };

        if let Some(error) = context_write_error.lock().unwrap().take() {
            // Keep the operation's recovery frames and context enqueue intact.
            return Err(HarnessError::io(error));
        }

        // Persist new messages + derive outcome.
        let mut outcome = match result {
            Ok(new_messages) => {
                let cut = post_compaction_cut.load(Ordering::Acquire);
                let start = cut.min(new_messages.len());
                let (leaf, final_id) = self
                    .persist_new_messages(&new_messages[start..], start, &settle_state)
                    .await?;
                // A refused final ACK held AgentEnd at the settle barrier. All
                // loop messages are durable now; release through the original
                // recorder, without handling/settling the event a second time.
                settle_state
                    .release_pending_agent_end(frame_recorder.as_ref())
                    .await;
                // A budget stop ends the run "Completed" (the loop's normal exit
                // path), which would otherwise look like a finished task. Record
                // *why* it stopped so the user sees it in the transcript and the
                // next turn's context knows the work was cut short.
                // Bind the reason before awaiting: the `MutexGuard` is not
                // `Send`, and holding it across `append_message` would make the
                // whole run future non-`Send`.
                let budget_stop = run_budget.lock().unwrap().stop();
                if let Some(stop) = budget_stop {
                    let notice = crate::messages::create_custom_message(
                        "runBudget",
                        UserContent::Text(stop.message()),
                        true,
                        None,
                        now_ms(),
                    );
                    if let Err(error) = self.session.view(&self.lane).append_message(notice).await {
                        tracing::warn!(
                            lane = %self.lane,
                            %error,
                            "could not record the run-budget stop notice"
                        );
                        self.record_write_failure("the run-budget stop notice", &error);
                    }
                }
                let outcome = Self::derive_outcome(&new_messages, leaf.clone(), final_id.clone());
                // The run's messages are durable now, so its frames are no
                // longer needed as progress. Retiring them here (rather than at
                // each `MessageEnd`) is what keeps a *failed retry attempt*'s
                // frames from being salvaged as if they were real history.
                if let Err(error) = frame_recorder.clear().await {
                    tracing::warn!(lane = %self.lane, %error, "could not clear assistant frames");
                }
                outcome
            }
            Err(e) => {
                // A broken stream may leave its last deltas in the writer's
                // queue without ever emitting MessageEnd. Join that writer
                // before reading progress; otherwise terminal history could
                // seal the run before those deltas become recoverable. If the
                // flush fails, leave the operation open instead.
                frame_recorder
                    .flush_with_reason(crate::frame_progress::FrameFlushReason::ErrorRecovery)
                    .await?;
                // Settled messages may already be durable. Repair only the
                // remaining streams, pairing unresolved calls with synthetic
                // error results so the transcript stays a valid request.
                let recovery_tools = self.recovery_tools();
                Self::repair_interrupted_run(
                    &self.session,
                    &self.lane,
                    frame_recorder.run_id(),
                    &recovery_tools,
                    &signal,
                    true,
                )
                .await?;
                // Every recovered entry is durable now. Retirement failure is
                // safe here: startup/switch cleanup can retry it using the
                // OperationFinished proof written below, without replaying.
                if let Err(error) = frame_recorder.clear().await {
                    tracing::warn!(lane = %self.lane, %error, "could not clear recovered assistant frames");
                }
                let leaf_after_salvage = self
                    .session
                    .view(&self.lane)
                    .get_leaf_id()
                    .await
                    .map_err(session_to_harness_err)?
                    .unwrap_or_default();
                let error = OperationError {
                    code: "agent_error".into(),
                    message: e.to_string(),
                };
                HarnessRunOutcome::Failed {
                    leaf_id: leaf_after_salvage,
                    error,
                    final_entry_id: None,
                    final_message: None,
                }
            }
        };

        // Suspended operations remain open; all terminal paths share one ACK.
        self.finish_run_result(&run_id, &mut outcome).await?;

        Ok(RunResult { run_id, outcome })
    }

    /// Explicit compaction. Drives `prepare_compaction` + `compact` and
    /// persists the result entry.
    async fn compact_core(
        &self,
        custom_instructions: Option<String>,
    ) -> HarnessResult<CompactionResult> {
        let (run_id, signal, _idle) = self.acquire_run(OperationKind::Compaction).await?;
        let _run_lease = ActiveRunLease {
            harness: self,
            run_id: run_id.clone(),
        };
        let recovery_tools = self.recovery_tools();
        // `true`: a run starting here retries through its own retry loop, so a
        // leftover failed attempt is settled the ordinary way.
        Self::finish_interrupted_operation(
            &self.session,
            &self.lane,
            &recovery_tools,
            &signal,
            true,
            Some(&self.write_failures),
        )
        .await?;
        {
            let _gate = self.message_writes.lock().await;
            if let Some(active) = self.inner.lock().unwrap().active_run.as_mut() {
                active.recovering = false;
            }
        }

        let source_leaf = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;

        // Write operation_started.
        let result_entry_id = self.session.id_generator().next();
        let intent = OperationIntent::Compaction {
            custom_instructions: custom_instructions.clone(),
            result_entry_id: result_entry_id.clone(),
        };
        self.write_operation_started(&run_id, source_leaf.clone(), intent)
            .await?;

        let snap = self.snapshot_config()?;
        let path = self.branch_path_oldest_first().await?;

        let outcome = if path.is_empty() {
            CompactionOutcome::Declined {
                leaf_id: source_leaf.unwrap_or_default(),
            }
        } else {
            match prepare_compaction(&path, snap.compaction) {
                Ok(None) => CompactionOutcome::Declined {
                    leaf_id: source_leaf.unwrap_or_default(),
                },
                Ok(Some(prep)) => {
                    let provider = match Self::resolve_provider(&snap.models, &snap.model) {
                        Ok(p) => p,
                        Err(_) => match snap.models.first() {
                            Some(p) => Arc::clone(p),
                            None => {
                                let err = OperationError {
                                    code: "no_provider".into(),
                                    message: "No provider for compaction".into(),
                                };
                                self.write_operation_finished(
                                    &run_id,
                                    OperationOutcome::Failed,
                                    Some(err.clone()),
                                )
                                .await?;
                                self.finish_context_writes(&run_id, None).await?;
                                return Ok(CompactionResult {
                                    run_id,
                                    outcome: CompactionOutcome::Failed {
                                        leaf_id: source_leaf.unwrap_or_default(),
                                        error: err,
                                    },
                                });
                            }
                        },
                    };
                    let llm_opts = CompactionLlmOptions {
                        provider,
                        model: snap.model.clone(),
                        api_key: None,
                        signal: signal.clone(),
                        thinking_level: Some(snap.thinking_level),
                        retry: if snap.retry.enabled {
                            Some(snap.retry.clone())
                        } else {
                            None
                        },
                        custom_instructions: custom_instructions.clone(),
                        stream_options: snap.stream_options.clone(),
                    };
                    match {
                        let _progress = crate::events::CompactionProgressGuard::new(
                            &self.bus, &self.lane, true,
                        );
                        compact(&prep, &llm_opts).await
                    } {
                        Ok(result) => {
                            let entry = self
                                .persist_compaction_entry(
                                    &run_id,
                                    &result,
                                    // `/compact` and the compaction command run when
                                    // the user asks for them.
                                    CompactionReason::Manual,
                                )
                                .await?;
                            let leaf = entry.base().id.clone();
                            self.write_operation_finished(
                                &run_id,
                                OperationOutcome::Completed,
                                None,
                            )
                            .await?;
                            CompactionOutcome::Completed {
                                leaf_id: leaf,
                                entry,
                            }
                        }
                        Err(e) => {
                            let err = OperationError {
                                code: "compaction_failed".into(),
                                message: e.message.clone(),
                            };
                            self.write_operation_finished(
                                &run_id,
                                OperationOutcome::Failed,
                                Some(err.clone()),
                            )
                            .await?;
                            CompactionOutcome::Failed {
                                leaf_id: source_leaf.unwrap_or_default(),
                                error: err,
                            }
                        }
                    }
                }
                Err(e) => {
                    let err = OperationError {
                        code: "prepare_failed".into(),
                        message: e.message.clone(),
                    };
                    self.write_operation_finished(
                        &run_id,
                        OperationOutcome::Failed,
                        Some(err.clone()),
                    )
                    .await?;
                    CompactionOutcome::Failed {
                        leaf_id: source_leaf.unwrap_or_default(),
                        error: err,
                    }
                }
            }
        };

        if matches!(outcome, CompactionOutcome::Declined { .. }) {
            self.write_operation_finished(&run_id, OperationOutcome::Declined, None)
                .await?;
        }
        self.finish_context_writes(&run_id, None).await?;
        Ok(CompactionResult { run_id, outcome })
    }
}

/// Config snapshot cloned out of `HarnessInner` at the start of an operation.
struct ConfigSnapshot {
    child_scope: CancellationToken,
    parent_signal: Option<CancellationToken>,
    model: Model,
    thinking_level: ThinkingLevel,
    active_tool_names: Vec<String>,
    tools: Vec<HarnessTool>,
    system_prompt: Option<String>,
    resources: AgentHarnessResources,
    stream_options: AgentHarnessStreamOptions,
    retry: RetryPolicy,
    compaction: CompactionSettings,
    tool_execution: HarnessToolExecution,
    driving: DrivingMode,
    models: Vec<Arc<dyn AiProvider>>,
    to_provider_messages: Option<ConvertToLlm>,
    entry_projectors: BTreeMap<String, CustomEntryContextMessageProjector>,
    entry_transforms: Vec<ContextEntryTransform>,
    agent_emitter: Option<Arc<dyn AgentEmitter>>,
    before_tool_call: Option<BeforeToolCall>,
    after_tool_call: Option<AfterToolCall>,
    transform_context: Option<TransformContext>,
    provider_hooks: Option<Arc<dyn rpi_ai::ProviderHooks>>,
    steering_queue: SharedMessageQueue,
    follow_up_queue: SharedMessageQueue,
}

// ===========================================================================
// AgentLane impl for AgentHarness (the main lane)
// ===========================================================================

async fn normalize_user_images(mut message: AgentMessage) -> HarnessResult<AgentMessage> {
    let has_images = matches!(&message, AgentMessage::User(user)
        if matches!(&user.content, UserContent::Blocks(blocks)
            if blocks.iter().any(|block| matches!(block, Content::Image(_)))));
    if !has_images {
        return Ok(message);
    }
    #[cfg(test)]
    let test_gate = image_normalization_tests::gate_for_message(&message);
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        let _worker = test_gate.map(|gate| gate.enter_worker());
        if let AgentMessage::User(user) = &mut message {
            if let UserContent::Blocks(blocks) = &mut user.content {
                rpi_tools::image_processing::normalize_image_blocks(blocks);
            }
        }
        message
    })
    .await
    .map_err(|error| HarnessError::agent(format!("Image processing failed: {error}")))
}

#[async_trait::async_trait]
impl AgentLane for AgentHarness {
    async fn send_message(
        &self,
        message: AgentMessage,
        options: SendMessageOptions,
    ) -> HarnessResult<SendMessageResult> {
        AgentHarness::send_message(self, message, options).await
    }
    async fn has_pending_resume(&self) -> bool {
        self.pending_resume().is_some()
    }

    async fn resume_pending(&self) -> HarnessResult<Option<RunResult>> {
        AgentHarness::resume_pending(self).await
    }

    fn name(&self) -> &str {
        &self.lane
    }

    async fn get_leaf_id(&self) -> HarnessResult<Option<String>> {
        self.session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    async fn prompt_text(
        &self,
        text: &str,
        images: Vec<rpi_ai::types::ImageContent>,
    ) -> HarnessResult<RunResult> {
        let content = if images.is_empty() {
            UserContent::Text(text.to_string())
        } else {
            let mut blocks: Vec<Content> = Vec::with_capacity(images.len() + 1);
            blocks.push(Content::text(text));
            for img in images {
                blocks.push(Content::Image(img));
            }
            UserContent::Blocks(blocks)
        };
        let message = AgentMessage::User(UserMessage::new(content, now_ms()));
        self.run_core(vec![message]).await
    }

    async fn prompt_message(&self, message: AgentMessage) -> HarnessResult<RunResult> {
        self.run_core(vec![message]).await
    }

    async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> HarnessResult<RunResult> {
        self.run_core(messages).await
    }

    async fn skill(
        &self,
        name: &str,
        additional_instructions: Option<&str>,
    ) -> HarnessResult<RunResult> {
        // Scope the `std::sync::MutexGuard` so it is provably dropped before the
        // `.await` below (a !Send guard cannot cross an await point in a
        // `Send` future).
        let message = {
            let inner = self.inner.lock().unwrap();
            let skills: &[Skill] = inner.resources.skills.as_deref().unwrap_or(&[]);
            let skill = skills.iter().find(|s| s.name == name).ok_or_else(|| {
                HarnessError::unknown_skill(name, format!("No skill named '{name}'"))
            })?;
            let invocation = format_skill_invocation(skill, additional_instructions);
            AgentMessage::User(UserMessage::new(invocation, now_ms()))
        };
        self.run_core(vec![message]).await
    }

    async fn prompt_from_template(&self, name: &str, args: &[String]) -> HarnessResult<RunResult> {
        let message = {
            let inner = self.inner.lock().unwrap();
            let templates: &[PromptTemplate] =
                inner.resources.prompt_templates.as_deref().unwrap_or(&[]);
            let template = templates.iter().find(|t| t.name == name).ok_or_else(|| {
                HarnessError::unknown_template(name, format!("No prompt template named '{name}'"))
            })?;
            let invocation = format_prompt_template_invocation(template, args);
            AgentMessage::User(UserMessage::new(invocation, now_ms()))
        };
        self.run_core(vec![message]).await
    }

    async fn compact(&self, custom_instructions: Option<&str>) -> HarnessResult<CompactionResult> {
        self.compact_core(custom_instructions.map(|s| s.to_string()))
            .await
    }

    async fn navigate_tree(
        &self,
        target_id: Option<&str>,
        _summarize: bool,
        _custom_instructions: Option<&str>,
        _label: Option<&str>,
    ) -> HarnessResult<NavigationResult> {
        let _gate = self.message_writes.lock().await;
        {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            if let Some(active) = &inner.active_run {
                return Err(HarnessError::lane_busy(
                    self.lane.clone(),
                    active.run_id.clone(),
                    active.kind,
                    "Wait for the active operation to finish before navigating",
                ));
            }
        }
        let run_id = self.session.id_generator().next();
        let target = target_id.filter(|id| !id.is_empty());
        match self.session.move_lane(&self.lane, target).await {
            Ok(()) => {
                let leaf = self
                    .session
                    .view(&self.lane)
                    .get_leaf_id()
                    .await
                    .map_err(session_to_harness_err)?;
                Ok(NavigationResult {
                    run_id,
                    outcome: NavigationOutcome::Completed {
                        new_leaf_id: leaf,
                        summary_entry: None,
                    },
                })
            }
            Err(error) => Ok(NavigationResult {
                run_id,
                outcome: NavigationOutcome::Failed {
                    leaf_id: self
                        .session
                        .view(&self.lane)
                        .get_leaf_id()
                        .await
                        .ok()
                        .flatten(),
                    error: OperationError {
                        code: "navigation_failed".into(),
                        message: error.to_string(),
                    },
                },
            }),
        }
    }

    async fn abort(&self) -> HarnessResult<AbortResult> {
        // Cancel **and** reclaim under the same write gate.
        //
        // The agent loop drains the steering queue when it observes the
        // cancellation and emits those messages as transcript content. Doing the
        // reclaim in a separate `clear_queue()` call would leave a window: a
        // message enqueued just before is handed back *and* drained, so a remote
        // UI restores and resends it. Both the drain hooks
        // (`get_steering_messages` / `get_follow_up_messages`) and `clear_queue`
        // take `message_writes`, so holding it across the cancel excludes every
        // enqueue/drain for the whole transition.
        self.abort_reclaiming(false).await
    }

    async fn steer(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        // Mirror upstream's steering queue: enqueue unconditionally, even when
        // the run has ended. The loop drains via `getSteeringMessages` at each
        // tool-batch boundary, and a message that lands after the run finishes
        // stays queued for the next explicit run. This avoids the race between
        // `activeRun` clearing and the TUI's status check that used to send
        // messages to `next_run_queue` (which never auto-wakes).
        self.enqueue_message(normalize_user_images(message).await?, QueueKind::Steer)
            .await
    }

    async fn follow_up(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        // Same as steer: unconditional enqueue, drained by `getFollowUpMessages`
        // after the loop would otherwise stop.
        self.enqueue_message(normalize_user_images(message).await?, QueueKind::FollowUp)
            .await
    }

    async fn next_run(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        self.enqueue_message(normalize_user_images(message).await?, QueueKind::NextRun)
            .await
    }

    async fn cancel_queued(&self, entry_id: &str) -> HarnessResult<CancelQueuedResult> {
        self.cancel_queue_item(entry_id).await
    }

    async fn queued_messages(&self) -> HarnessResult<QueuedMessages> {
        let _gate = self.message_writes.lock().await;
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        let steering = inner
            .steering_queue
            .lock()
            .unwrap()
            .peek_for_lane(&self.lane);
        let follow_up = inner
            .follow_up_queue
            .lock()
            .unwrap()
            .peek_for_lane(&self.lane);
        Ok(QueuedMessages {
            steering,
            follow_up,
        })
    }

    async fn clear_queue(&self) -> HarnessResult<QueuedMessages> {
        self.clear_queue_items().await
    }

    async fn record_usage(
        &self,
        usage: Usage,
        entry_id: Option<&str>,
        details: Option<JsonValue>,
    ) -> HarnessResult<RecordUsageResult> {
        // Keep the operation association and write atomic with session swaps
        // and operation_finished. End callbacks still hold admission, but no
        // longer own an open durable operation to associate usage with.
        let _gate = self.message_writes.lock().await;
        // Scope the !Send `std::sync::MutexGuard` before the `.await`.
        let run_id = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner
                .active_run
                .as_ref()
                .filter(|active| active.lane == self.lane && !active.finishing)
                .map(|active| active.run_id.clone())
        };
        // Admission also covers preparation before operation_started is
        // durable. Usage from that phase is a lane fact, not a run record.
        let run_id = if let Some(run_id) = run_id {
            self.session
                .find_open_operations(&self.lane, None)
                .await
                .map_err(session_to_harness_err)?
                .iter()
                .any(|operation| operation.base.id == run_id)
                .then_some(run_id)
        } else {
            None
        };

        let record = LaneRecord::Usage(UsageRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            usage,
            cause: UsageCause::Assistant,
            run_id,
            entry_id: entry_id.map(|s| s.to_string()),
            attempt: None,
            stop_reason: None,
            tool_call_id: None,
            details,
        });
        self.session
            .append_record(record)
            .await
            .map_err(session_to_harness_err)?;
        Ok(RecordUsageResult)
    }

    async fn wait_for_idle(&self) -> HarnessResult<()> {
        loop {
            let active = {
                let inner = self.inner.lock().unwrap();
                if inner.closed {
                    return Err(HarnessError::closed());
                }
                inner
                    .active_run
                    .as_ref()
                    .map(|active| (active.run_id.clone(), Arc::clone(&active.idle)))
            };
            let Some((run_id, notify)) = active else {
                return Ok(());
            };
            let notified = notify.notified();
            tokio::pin!(notified);
            // Register before rechecking: notify_waiters does not retain a
            // permit for a future created after the operation finished.
            notified.as_mut().enable();
            if !self
                .inner
                .lock()
                .unwrap()
                .active_run
                .as_ref()
                .is_some_and(|active| active.run_id == run_id)
            {
                continue;
            }
            notified.await;
        }
    }

    /// Forward to the backend through the session facade.
    async fn drain_session(&self) {
        AgentHarness::drain_session(self).await
    }

    fn take_write_failures(&self) -> Vec<HarnessWriteFailure> {
        AgentHarness::take_write_failures(self)
    }

    async fn run_when_idle(
        &self,
        callback: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> HarnessResult<()> {
        let is_idle = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner.active_run.is_none()
        };
        if is_idle {
            callback().await;
        } else {
            self.wait_for_idle().await?;
            callback().await;
        }
        Ok(())
    }

    async fn get_model(&self) -> HarnessResult<Model> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.model.clone())
    }

    async fn set_model(&self, model: Model) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.model = model;
        Ok(())
    }

    async fn get_thinking_level(&self) -> HarnessResult<ThinkingLevel> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.thinking_level)
    }

    async fn set_thinking_level(&self, level: ThinkingLevel) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.thinking_level = level;
        Ok(())
    }

    async fn get_active_tools(&self) -> HarnessResult<Vec<String>> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.active_tool_names.clone())
    }

    async fn set_active_tools(&self, names: Vec<String>) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        if inner.exact_resources {
            return Err(HarnessError::io(
                "set_active_tools unsupported with exact resources; changes require a new session and restart",
            ));
        }
        inner.active_tool_names = names;
        Ok(())
    }

    fn session_view(&self) -> Arc<dyn SessionTree> {
        self.session.view(&self.lane)
    }

    async fn get_tip_id(&self) -> HarnessResult<Option<String>> {
        AgentHarness::get_tip_id(self).await
    }

    async fn find_entries(&self, query: &EntryQuery) -> HarnessResult<Vec<Entry>> {
        AgentHarness::find_entries(self, query).await
    }

    async fn find_entry(&self, query: &EntryQuery) -> HarnessResult<Option<Entry>> {
        AgentHarness::find_entry(self, query).await
    }

    async fn get_entry(&self, id: &str) -> HarnessResult<Option<Entry>> {
        AgentHarness::get_entry(self, id).await
    }

    async fn append_message(&self, message: AgentMessage) -> HarnessResult<String> {
        AgentHarness::append_message(self, message).await
    }

    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> HarnessResult<String> {
        AgentHarness::append_custom_entry(self, custom_type, data).await
    }

    async fn get_name(&self) -> HarnessResult<Option<String>> {
        AgentHarness::get_name(self).await
    }

    async fn set_name(&self, name: Option<&str>) -> HarnessResult<()> {
        AgentHarness::set_name(self, name).await
    }

    async fn get_label(&self, target_id: &str) -> HarnessResult<Option<String>> {
        AgentHarness::get_label(self, target_id).await
    }

    async fn set_label(&self, target_id: &str, label: Option<&str>) -> HarnessResult<()> {
        AgentHarness::set_label(self, target_id, label).await
    }

    async fn get_stats(&self) -> HarnessResult<SessionStats> {
        AgentHarness::get_stats(self).await
    }

    async fn snapshot(&self) -> HarnessResult<LaneSnapshot> {
        AgentHarness::snapshot(self).await
    }

    async fn active_operation(&self) -> HarnessResult<Option<ActiveOperationSnapshot>> {
        // Retain snapshot's authoritative lane/storage validation without
        // computing its best-effort labels, name, or statistics.
        self.view()
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;
        Ok(self.active_operation_snapshot())
    }

    async fn inspect_execution(
        &self,
        tool_call_id: &str,
    ) -> HarnessResult<Option<ToolExecutionInfo>> {
        AgentHarness::inspect_execution(self, tool_call_id).await
    }

    async fn get_result(&self, run_id: &str) -> HarnessResult<Option<RunResult>> {
        AgentHarness::get_result(self, run_id).await
    }

    async fn accept(&self, entry_id: &str) -> HarnessResult<bool> {
        AgentHarness::accept(self, entry_id).await
    }

    async fn request_abort(&self, run_id: &str) -> HarnessResult<bool> {
        AgentHarness::request_abort(self, run_id).await
    }

    async fn drive(&self, operation_id: &str) -> HarnessResult<DriveResult> {
        AgentHarness::drive(self, operation_id).await
    }

    async fn resume(&self, suspended_id: &str) -> HarnessResult<RunResult> {
        AgentHarness::resume(self, suspended_id).await
    }
}

// ===========================================================================
// LaneHandle - legacy read-oriented compatibility wrapper. New callers should
// use `AgentHarness::lane`, which returns a fully executable lane runner.
// ===========================================================================

/// A non-main lane handle. Cheap to clone (shares the harness inner state).
#[derive(Clone)]
pub struct LaneHandle {
    session: Session,
    lane: String,
    view: Arc<dyn SessionTree>,
    inner: Arc<Mutex<HarnessInner>>,
    /// Kept so non-main lanes can register listeners/watch the bus once
    /// `watch`/`events` are surfaced for non-main lanes; v1 leaves it unused.
    #[allow(dead_code)]
    bus: HarnessEventBus,
    message_writes: Arc<tokio::sync::Mutex<()>>,
    write_failures: Arc<Mutex<Vec<HarnessWriteFailure>>>,
    unsaved_messages: Arc<Mutex<Vec<UnsavedMessage>>>,
    child_scope: Arc<Mutex<CancellationToken>>,
    is_child: bool,
    max_turns: Option<u32>,
    /// Shared with the owning harness so a stop request reaches the run loop.
    stop_round: Arc<std::sync::atomic::AtomicBool>,
}

impl LaneHandle {
    fn runner(&self) -> AgentHarness {
        AgentHarness {
            session: self.session.clone(),
            inner: Arc::clone(&self.inner),
            bus: self.bus.clone(),
            lane: self.lane.clone(),
            message_writes: Arc::clone(&self.message_writes),
            write_failures: Arc::clone(&self.write_failures),
            unsaved_messages: Arc::clone(&self.unsaved_messages),
            child_scope: self.child_scope.clone(),
            is_child: self.is_child,
            max_turns: self.max_turns,
            stop_round: Arc::clone(&self.stop_round),
        }
    }
}

#[async_trait::async_trait]
impl AgentLane for LaneHandle {
    async fn send_message(
        &self,
        message: AgentMessage,
        options: SendMessageOptions,
    ) -> HarnessResult<SendMessageResult> {
        self.runner().send_message(message, options).await
    }
    async fn has_pending_resume(&self) -> bool {
        self.runner().pending_resume().is_some()
    }

    async fn resume_pending(&self) -> HarnessResult<Option<RunResult>> {
        self.runner().resume_pending().await
    }

    fn name(&self) -> &str {
        &self.lane
    }

    async fn get_leaf_id(&self) -> HarnessResult<Option<String>> {
        self.view
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    async fn prompt_text(
        &self,
        text: &str,
        images: Vec<rpi_ai::types::ImageContent>,
    ) -> HarnessResult<RunResult> {
        self.runner().prompt_text(text, images).await
    }

    async fn prompt_message(&self, message: AgentMessage) -> HarnessResult<RunResult> {
        self.runner().prompt_message(message).await
    }

    async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> HarnessResult<RunResult> {
        self.runner().prompt_messages(messages).await
    }

    async fn skill(
        &self,
        name: &str,
        additional_instructions: Option<&str>,
    ) -> HarnessResult<RunResult> {
        self.runner().skill(name, additional_instructions).await
    }

    async fn prompt_from_template(&self, name: &str, args: &[String]) -> HarnessResult<RunResult> {
        self.runner().prompt_from_template(name, args).await
    }

    async fn compact(&self, custom_instructions: Option<&str>) -> HarnessResult<CompactionResult> {
        self.runner().compact(custom_instructions).await
    }

    async fn navigate_tree(
        &self,
        target_id: Option<&str>,
        summarize: bool,
        custom_instructions: Option<&str>,
        label: Option<&str>,
    ) -> HarnessResult<NavigationResult> {
        self.runner()
            .navigate_tree(target_id, summarize, custom_instructions, label)
            .await
    }

    async fn abort(&self) -> HarnessResult<AbortResult> {
        self.runner().abort().await
    }

    async fn steer(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        self.runner().steer(message).await
    }

    async fn follow_up(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        self.runner().follow_up(message).await
    }

    async fn next_run(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        self.runner().next_run(message).await
    }

    async fn cancel_queued(&self, entry_id: &str) -> HarnessResult<CancelQueuedResult> {
        self.runner().cancel_queued(entry_id).await
    }

    async fn queued_messages(&self) -> HarnessResult<QueuedMessages> {
        self.runner().queued_messages().await
    }

    async fn clear_queue(&self) -> HarnessResult<QueuedMessages> {
        self.runner().clear_queue().await
    }

    async fn record_usage(
        &self,
        usage: Usage,
        entry_id: Option<&str>,
        details: Option<JsonValue>,
    ) -> HarnessResult<RecordUsageResult> {
        // Scope the !Send `std::sync::MutexGuard` before the `.await`.
        let run_id = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner.active_run.as_ref().map(|a| a.run_id.clone())
        };

        let record = LaneRecord::Usage(UsageRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            usage,
            cause: UsageCause::Assistant,
            run_id,
            entry_id: entry_id.map(|s| s.to_string()),
            attempt: None,
            stop_reason: None,
            tool_call_id: None,
            details,
        });
        self.session
            .append_record(record)
            .await
            .map_err(session_to_harness_err)?;
        Ok(RecordUsageResult)
    }

    async fn wait_for_idle(&self) -> HarnessResult<()> {
        self.runner().wait_for_idle().await
    }

    /// Forward to the backend through the session facade.
    async fn drain_session(&self) {
        AgentHarness::drain_session(&self.runner()).await
    }

    fn take_write_failures(&self) -> Vec<HarnessWriteFailure> {
        AgentHarness::take_write_failures(&self.runner())
    }

    async fn run_when_idle(
        &self,
        callback: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> HarnessResult<()> {
        let is_idle = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner.active_run.is_none()
        };
        if is_idle {
            callback().await;
        } else {
            self.wait_for_idle().await?;
            callback().await;
        }
        Ok(())
    }

    async fn get_model(&self) -> HarnessResult<Model> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.model.clone())
    }

    async fn set_model(&self, model: Model) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.model = model;
        Ok(())
    }

    async fn get_thinking_level(&self) -> HarnessResult<ThinkingLevel> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.thinking_level)
    }

    async fn set_thinking_level(&self, level: ThinkingLevel) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.thinking_level = level;
        Ok(())
    }

    async fn get_active_tools(&self) -> HarnessResult<Vec<String>> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.active_tool_names.clone())
    }

    async fn set_active_tools(&self, names: Vec<String>) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        if inner.exact_resources {
            return Err(HarnessError::io(
                "set_active_tools unsupported with exact resources; changes require a new session and restart",
            ));
        }
        inner.active_tool_names = names;
        Ok(())
    }

    fn session_view(&self) -> Arc<dyn SessionTree> {
        Arc::clone(&self.view)
    }

    async fn get_tip_id(&self) -> HarnessResult<Option<String>> {
        self.view
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    async fn find_entries(&self, query: &EntryQuery) -> HarnessResult<Vec<Entry>> {
        self.view
            .find_entries(query)
            .await
            .map_err(session_to_harness_err)
    }

    async fn find_entry(&self, query: &EntryQuery) -> HarnessResult<Option<Entry>> {
        self.view
            .find_entry(query)
            .await
            .map_err(session_to_harness_err)
    }

    async fn get_entry(&self, id: &str) -> HarnessResult<Option<Entry>> {
        self.view
            .get_entry(id)
            .await
            .map_err(session_to_harness_err)
    }

    async fn append_message(&self, message: AgentMessage) -> HarnessResult<String> {
        self.runner().append_message(message).await
    }

    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> HarnessResult<String> {
        self.view
            .append_custom_entry(custom_type, data)
            .await
            .map_err(session_to_harness_err)
    }

    async fn get_name(&self) -> HarnessResult<Option<String>> {
        self.view.get_name().await.map_err(session_to_harness_err)
    }

    async fn set_name(&self, name: Option<&str>) -> HarnessResult<()> {
        self.view
            .set_name(name)
            .await
            .map_err(session_to_harness_err)
    }

    async fn get_label(&self, target_id: &str) -> HarnessResult<Option<String>> {
        self.view
            .get_label(target_id)
            .await
            .map_err(session_to_harness_err)
    }

    async fn set_label(&self, target_id: &str, label: Option<&str>) -> HarnessResult<()> {
        self.view
            .set_label(target_id, label)
            .await
            .map_err(session_to_harness_err)
    }

    async fn get_stats(&self) -> HarnessResult<SessionStats> {
        self.view.get_stats().await.map_err(session_to_harness_err)
    }

    async fn snapshot(&self) -> HarnessResult<LaneSnapshot> {
        self.runner().snapshot().await
    }

    async fn active_operation(&self) -> HarnessResult<Option<ActiveOperationSnapshot>> {
        self.view
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;
        Ok(self.runner().active_operation_snapshot())
    }

    async fn inspect_execution(
        &self,
        tool_call_id: &str,
    ) -> HarnessResult<Option<ToolExecutionInfo>> {
        self.runner().inspect_execution(tool_call_id).await
    }

    async fn get_result(&self, run_id: &str) -> HarnessResult<Option<RunResult>> {
        self.runner().get_result(run_id).await
    }

    async fn accept(&self, entry_id: &str) -> HarnessResult<bool> {
        self.runner().accept(entry_id).await
    }

    async fn request_abort(&self, run_id: &str) -> HarnessResult<bool> {
        self.runner().request_abort(run_id).await
    }

    async fn drive(&self, operation_id: &str) -> HarnessResult<DriveResult> {
        self.runner().drive(operation_id).await
    }

    async fn resume(&self, suspended_id: &str) -> HarnessResult<RunResult> {
        self.runner().resume(suspended_id).await
    }
}

#[cfg(test)]
#[path = "agent_harness/image_normalization_tests.rs"]
mod image_normalization_tests;
#[cfg(test)]
mod admission_window_tests {
    use super::*;

    /// A `StopRound` issued after the run is admitted but before the loop starts
    /// must survive to the first turn boundary.
    ///
    /// Admission makes the run visible (`active_run` is set), so a caller that
    /// observes "this lane is busy" may legitimately issue `StopRound` for it.
    /// The idle-request reset therefore has to happen while the run is still
    /// invisible — inside `acquire_run_locked`, under `message_writes` — not
    /// after admission, where it would erase exactly that request and cost an
    /// extra provider call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stop_round_issued_between_admission_and_the_loop_is_honored() {
        // A tool call needs a second turn, so a lost request shows up as a
        // second provider call rather than as timing noise.
        let script = rpi_ai::providers::faux::FauxScript::new()
            .with_tool_call("bash", serde_json::json!({ "command": "true" }));
        script.append_responses(vec![rpi_ai::providers::faux::FauxStep::text("second turn")]);
        let provider = rpi_ai::providers::faux::FauxProvider::new(script);
        let harness = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            ..Default::default()
        })
        .await
        .unwrap();

        // Walk the production admission path by hand, then issue the request in
        // the gap before the loop runs.
        let admission = harness.acquire_run(OperationKind::Run).await.unwrap();
        harness.request_stop_round();
        let _ = harness
            .run_core_with_admission(Vec::new(), false, admission, None)
            .await;

        assert_eq!(
            provider
                .state()
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a StopRound issued in the admission window must stop before another turn"
        );
    }
}

#[cfg(test)]
mod resumable_probe_tests;

#[cfg(test)]
mod queue_tests {
    use super::*;
    use rpi_ai::types::UserMessage;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage::new(text, 0))
    }

    #[test]
    fn queue_mode_one_at_a_time_preserves_entry_order() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime);
        queue.pending.push_back(QueuedMessage {
            entry_id: "a".into(),
            lane: "main".into(),
            message: user("first"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "b".into(),
            lane: "main".into(),
            message: user("second"),
        });
        assert_eq!(queue.drain_for_lane("main").len(), 1);
        assert_eq!(
            queue.pending.front().map(|item| item.entry_id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn queue_cancel_removes_only_the_requested_entry() {
        let mut queue = MessageQueue::new(QueueMode::All);
        queue.pending.push_back(QueuedMessage {
            entry_id: "a".into(),
            lane: "main".into(),
            message: user("first"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "b".into(),
            lane: "main".into(),
            message: user("second"),
        });
        assert!(queue.remove("a"));
        assert!(!queue.remove("missing"));
        assert_eq!(
            queue.pending.front().map(|item| item.entry_id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn queue_drain_isolated_by_lane() {
        let mut queue = MessageQueue::new(QueueMode::All);
        queue.pending.push_back(QueuedMessage {
            entry_id: "main-1".into(),
            lane: "main".into(),
            message: user("main"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "side-1".into(),
            lane: "side".into(),
            message: user("side"),
        });
        assert_eq!(queue.drain_for_lane("main").len(), 1);
        assert_eq!(
            queue.pending.front().map(|item| item.entry_id.as_str()),
            Some("side-1")
        );
        assert_eq!(queue.drain_for_lane("side").len(), 1);
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn queue_peek_reports_lane_messages_without_consuming() {
        let mut queue = MessageQueue::new(QueueMode::All);
        queue.pending.push_back(QueuedMessage {
            entry_id: "a".into(),
            lane: "main".into(),
            message: user("first"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "b".into(),
            lane: "side".into(),
            message: user("other"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "c".into(),
            lane: "main".into(),
            message: user("second"),
        });
        let peeked = queue.peek_for_lane("main");
        assert_eq!(
            peeked.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        // Peeking must not change the queue.
        assert_eq!(queue.pending.len(), 3);
    }

    #[test]
    fn queue_take_for_lane_drains_every_lane_entry() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime);
        queue.pending.push_back(QueuedMessage {
            entry_id: "a".into(),
            lane: "main".into(),
            message: user("first"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "b".into(),
            lane: "side".into(),
            message: user("other"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "c".into(),
            lane: "main".into(),
            message: user("second"),
        });
        // Unlike `drain_for_lane` (which honours `QueueMode`), the dequeue
        // action clears the lane completely regardless of mode. It returns items
        // (ids included) so the caller can record each cancellation.
        let taken: Vec<String> = queue
            .take_items_for_lane("main")
            .into_iter()
            .map(|item| queued_message_text(&item.message))
            .collect();
        assert_eq!(taken, vec!["first", "second"]);
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(queue.pending[0].entry_id, "b");
    }
}

// ===========================================================================
// Free helpers
// ===========================================================================

/// Map a [`SessionError`] to a [`HarnessError`]. Session storage failures
/// surface as `Io`; invalid-lane errors surface as `InvalidLane`.
fn session_to_harness_err(e: SessionError) -> HarnessError {
    match e.code {
        SessionErrorCode::InvalidLane => HarnessError::invalid_lane("main", "session", e.message),
        // The backend refused the write. Kept distinct from `Io` so the host can
        // tell the user to export their transcript instead of retrying a request
        // that was never the problem.
        SessionErrorCode::Storage => HarnessError::session_storage(e.message),
        _ => HarnessError::io(e.message),
    }
}

/// Current time in unix milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod session_switch_tests {
    use super::*;
    use crate::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use crate::session::memory::{InMemorySessionStorage, SystemClock};
    use crate::session::session::DefaultIdGenerator;
    use crate::session::types::SessionMetadata;
    use rpi_ai::{Api, Model};
    use rpi_tools::{FileSystem, OsExecutionEnv};

    #[tokio::test]
    async fn lightweight_active_operation_matches_snapshot_for_every_kind_and_after_release() {
        let harness = AgentHarness::create(opts()).await.unwrap();
        let lane = harness.lane("main");
        assert!(harness.lane("missing").snapshot().await.is_err());
        assert!(harness.lane("missing").active_operation().await.is_err());
        for kind in [
            OperationKind::Run,
            OperationKind::Compaction,
            OperationKind::Navigation,
        ] {
            let (run_id, _, _) = harness.acquire_run(kind).await.unwrap();
            let expected = harness.snapshot().await.unwrap().active.unwrap();
            for actual in [
                AgentLane::active_operation(&harness)
                    .await
                    .unwrap()
                    .unwrap(),
                lane.active_operation().await.unwrap().unwrap(),
            ] {
                assert_eq!(actual.run_id, expected.run_id);
                assert_eq!(actual.lane, expected.lane);
                assert_eq!(actual.kind, expected.kind);
            }
            harness.release_run(&run_id);
            assert!(AgentLane::active_operation(&harness)
                .await
                .unwrap()
                .is_none());
            assert!(lane.active_operation().await.unwrap().is_none());
        }
    }

    fn opts() -> AgentHarnessOptions {
        AgentHarnessOptions {
            model: Model::new("m", "m", Api::Faux, "faux", "http://x"),
            thinking_level: Default::default(),
            active_tool_names: Vec::new(),
            tools: Vec::new(),
            system_prompt: None,
            resources: AgentHarnessResources::default(),
            stream_options: Default::default(),
            retry: RetryPolicy::default(),
            compaction: CompactionSettings::default(),
            steering_mode: Default::default(),
            follow_up_mode: Default::default(),
            tool_execution: HarnessToolExecution::default(),
            drive: DrivingMode::default(),
            session: Session::new(
                Arc::new(InMemorySessionStorage::new(
                    SessionMetadata {
                        id: "a".into(),
                        created_at: 0,
                        parent_session_id: None,
                    },
                    Arc::new(SystemClock),
                    Arc::new(DefaultIdGenerator::new()),
                )),
                None,
            ),
            allow_existing_session: true,
            models: Vec::new(),
            to_provider_messages: None,
            entry_projectors: Default::default(),
            agent_emitter: None,
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            entry_transforms: Vec::new(),
            provider_hooks: None,
        }
    }

    /// The host must be able to wait for the session's durable writes.
    ///
    /// The backend's wait existed but was unreachable: `JsonlSessionStorage::drain`
    /// was public on the concrete type, while the TUI only holds `dyn AgentLane`.
    /// Without this entry point the shutdown path had nothing to call, so it
    /// aborted the tasks carrying a turn's writes without waiting — the wait was
    /// dead code and the "in-flight append" contract covered nothing.
    ///
    /// This asserts the call is reachable from a lane and is safe to make on an
    /// idle session (no deadlock on the shutdown path). The backend's own probes
    /// cover what the wait actually waits for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lane_can_wait_for_the_sessions_durable_writes() {
        let harness = AgentHarness::create(opts()).await.unwrap();
        let lane = harness.lane("main");

        tokio::time::timeout(std::time::Duration::from_secs(5), lane.drain_session())
            .await
            .expect("draining an idle lane must not block the shutdown path");

        // A write admitted through the harness must be settled by the time the
        // wait returns, so a shutdown that follows cannot drop it.
        harness
            .append_message(crate::messages::create_custom_message(
                "shutdown",
                UserContent::Text("written before exit".into()),
                true,
                None,
                0,
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), lane.drain_session())
            .await
            .expect("draining after a write must complete");
    }

    /// A failed session write must survive as data the host can render.
    ///
    /// `tracing::warn!` inside an alternate screen reaches nobody, so the queue is
    /// the only channel by which "the conversation stopped being saved" can reach
    /// the user. This pins the recording and the drain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_write_is_recorded_and_drained_once() {
        let harness = AgentHarness::create(opts()).await.unwrap();
        assert!(harness.take_write_failures().is_empty());

        harness.record_write_failure(
            "a salvaged assistant message",
            &SessionError::storage("disk full"),
        );
        let drained = harness.take_write_failures();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].what, "a salvaged assistant message");
        assert_eq!(drained[0].message, "disk full");

        // Drained, not peeked at: a second call must be empty, otherwise the host
        // would re-report the same failure on every turn boundary.
        assert!(harness.take_write_failures().is_empty());
    }

    /// A real backend failure must reach the queue, not just the log.
    ///
    /// The probes above call `record_write_failure` directly, which only proves
    /// the queue works. This one drives the production path: the JSONL writer
    /// refuses the append, `append_message` returns `Err`, and the harness has to
    /// record it. Without this, "the transcript shows the failure" would rest on
    /// a hand-wired call that a future edit could drop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_append_is_reported_as_a_write_failure() {
        /// Refuses every append, the way a full disk does.
        struct RefusingAppend;

        #[async_trait::async_trait]
        impl rpi_tools::env::AppendFile for RefusingAppend {
            async fn append(&mut self, _: &[u8]) -> Result<(), rpi_tools::FileError> {
                Err(rpi_tools::FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "disk full",
                ))
            }
        }

        let tmp =
            std::env::temp_dir().join(format!("rpi-write-failure-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let env = Arc::new(OsExecutionEnv::with_cwd(tmp.clone()));
        let fs: Arc<dyn FileSystem> = env.clone();
        let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
            fs,
            sessions_root: tmp.join("sessions").to_string_lossy().into_owned(),
            clock: Arc::new(SystemClock),
            ids: Arc::new(DefaultIdGenerator::new()),
        });
        let storage = repo
            .create_typed(&JsonlSessionCreateOptions {
                id: Some("sess-write".into()),
                parent_session_id: None,
                cwd: tmp.to_string_lossy().into_owned(),
                metadata: None,
            })
            .await
            .unwrap();
        storage.install_append_handle(Box::new(RefusingAppend));

        let mut options = opts();
        options.session = Session::new(Arc::new(storage), None);
        let harness = AgentHarness::create(options).await.unwrap();
        let lane = harness.lane("main");

        // A plain append through the harness must record the failure.
        let result = harness
            .append_message(crate::messages::create_custom_message(
                "probe",
                UserContent::Text("must not be saved".into()),
                true,
                None,
                0,
            ))
            .await;
        assert!(
            result.is_err(),
            "the refusing writer must surface its error"
        );
        let reported = lane.take_write_failures();
        assert!(
            reported.iter().any(|f| f.message.contains("disk full")),
            "a refused append must be queued for the host, got {reported:?}"
        );

        // The warning tells the user to export before exiting. That promise is
        // only true if the refused message has a copy somewhere the export can
        // read: JSONL storage applies an entry only after the append succeeds, so
        // the session view has none.
        let entries = harness
            .view()
            .find_entries(&EntryQuery::default())
            .await
            .unwrap();
        assert!(
            !format!("{entries:?}").contains("must not be saved"),
            "precondition: a refused write must not appear in the session view: {entries:?}"
        );
        let unsaved = harness.unsaved_messages();
        assert_eq!(unsaved.len(), 1, "the recovery copy must be retained");
        assert!(
            format!("{:?}", unsaved[0].message).contains("must not be saved"),
            "the retained copy must carry the refused message itself: {unsaved:?}"
        );
        assert!(
            unsaved[0].what.contains("a message you sent"),
            "the copy must say what could not be persisted: {unsaved:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A lane handle must see the failures the harness it spawned recorded.
    ///
    /// The TUI holds `Arc<dyn AgentLane>`, not the harness, so a per-lane copy of
    /// the queue would make every failure invisible on exactly the surface that
    /// renders them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lane_shares_the_write_failures_of_its_harness() {
        let harness = AgentHarness::create(opts()).await.unwrap();
        let lane = harness.lane("main");
        harness.record_write_failure("the run-budget stop notice", &SessionError::storage("EIO"));
        let drained = lane.take_write_failures();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].what, "the run-budget stop notice");
        assert!(harness.take_write_failures().is_empty());
    }

    /// A backend failing every write must not grow the queue without bound.
    ///
    /// The host drains at turn boundaries, but one run can fail many writes before
    /// reaching one, and an unbounded queue turns a disk problem into an
    /// out-of-memory one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_failures_are_bounded() {
        let harness = AgentHarness::create(opts()).await.unwrap();
        for _ in 0..(MAX_WRITE_FAILURES * 4) {
            harness.record_write_failure(
                "a salvaged assistant message",
                &SessionError::storage("disk full"),
            );
        }
        assert_eq!(harness.take_write_failures().len(), MAX_WRITE_FAILURES);
    }

    /// A refused write must not masquerade as a provider failure.
    ///
    /// Both used to collapse into `HarnessError::Io`, so the UI printed a model
    /// error for a full disk. The user's next action differs: retry a request
    /// versus export the transcript. The distinction is carried by the error tag
    /// (and by the `SessionStorage` code on a failed run outcome).
    #[test]
    fn a_storage_error_keeps_its_own_tag() {
        let storage = crate::error::SessionError::storage("disk full");
        let mapped = session_to_harness_err(storage);
        assert_eq!(mapped.tag(), crate::result::HarnessErrorTag::SessionStorage);

        // The neighbouring codes keep their existing mapping, so this cannot be
        // passing merely because every session error now reports the same tag.
        let not_found = crate::error::SessionError::new(
            crate::error::SessionErrorCode::NotFound,
            "no such entry",
        );
        assert_eq!(
            session_to_harness_err(not_found).tag(),
            crate::result::HarnessErrorTag::Io
        );
    }

    /// The tag must survive to the string the run outcome carries.
    ///
    /// `HarnessRunOutcome::Failed` carries `OperationError { code, message }`, and
    /// the TUI/print paths branch on `code`. If the constructors disagreed, the UI
    /// would key on a value the harness never produces.
    #[test]
    fn the_storage_tag_has_a_stable_wire_name() {
        assert_eq!(
            crate::result::HarnessErrorTag::SessionStorage.as_str(),
            "SessionStorage"
        );
        assert_eq!(
            HarnessError::session_storage("disk full").tag().as_str(),
            "SessionStorage"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_accepted_at_run_start_is_drained_even_without_a_provider() {
        let harness = AgentHarness::create(opts()).await.unwrap();
        let reserved = Arc::new(Mutex::new(None));
        let writer = harness.clone();
        let captured = Arc::clone(&reserved);
        let _subscription = harness.bus.on::<RunStartEvent, _>(move |_| {
            let id = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(writer.append_message(
                    crate::messages::create_custom_message(
                        "goal",
                        UserContent::Text("accepted context".into()),
                        true,
                        None,
                        0,
                    ),
                ))
            })
            .unwrap();
            *captured.lock().unwrap() = Some(id);
        });
        let result = harness.prompt_text("answer", Vec::new()).await.unwrap();
        let HarnessRunOutcome::Failed { leaf_id, error, .. } = result.outcome else {
            panic!("no provider")
        };
        assert_eq!(error.code, "no_provider");
        let id = reserved.lock().unwrap().clone().unwrap();
        assert_eq!(leaf_id, id);
        assert!(harness.get_entry(&id).await.unwrap().is_some());
        assert!(!harness.has_pending_context());
    }

    #[tokio::test]
    async fn clearing_image_queues_restores_attachments_without_losing_text() {
        // A complete 1x1 GIF, small enough to retain its original bytes.
        let image = rpi_ai::types::ImageContent {
            kind: rpi_ai::types::ImageContentType,
            data: "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7".into(),
            mime_type: "image/gif".into(),
        };
        let harness = AgentHarness::create(opts()).await.unwrap();
        let message = |text: &str| {
            AgentMessage::User(UserMessage::new(
                UserContent::Blocks(vec![Content::text(text), Content::Image(image.clone())]),
                0,
            ))
        };
        harness.steer(message("steer")).await.unwrap();
        harness.follow_up(message("follow")).await.unwrap();
        let restored = harness.clear_queue().await.unwrap();
        // Each entry keeps **its own** text and image together: a flat image
        // list could not express this pairing once two messages differ.
        assert_eq!(restored.steering.len(), 1);
        assert_eq!(restored.steering[0].text, "steer");
        assert_eq!(restored.steering[0].images, vec![image.clone()]);
        assert_eq!(restored.follow_up.len(), 1);
        assert_eq!(restored.follow_up[0].text, "follow");
        assert_eq!(restored.follow_up[0].images, vec![image]);
        assert!(harness.queued_messages().await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_is_reentrant_across_every_durable_commit_boundary() {
        struct FailAfter {
            remaining: usize,
            uncertain: bool,
            inner: Box<dyn rpi_tools::env::AppendFile>,
        }
        #[async_trait::async_trait]
        impl rpi_tools::env::AppendFile for FailAfter {
            async fn append(&mut self, bytes: &[u8]) -> Result<(), rpi_tools::FileError> {
                if self.remaining == 0 {
                    // Simulate both a definite failure and an append which reached
                    // disk but whose successful acknowledgement was lost.
                    if self.uncertain {
                        self.inner.append(bytes).await?;
                    }
                    return Err(rpi_tools::FileError::new(
                        rpi_tools::FileErrorCode::Unknown,
                        "injected recovery failure",
                    ));
                }
                self.remaining -= 1;
                self.inner.append(bytes).await
            }
        }
        for (tool_batch, uncertain) in [(false, false), (false, true), (true, false), (true, true)]
        {
            for fail_after in 0..=2 {
                let tmp = std::env::temp_dir().join(format!(
                    "rpi-recovery-reentry-{}-{fail_after}",
                    uuid::Uuid::now_v7()
                ));
                let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.clone()));
                let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
                    fs: fs.clone(),
                    sessions_root: tmp.to_string_lossy().into_owned(),
                    clock: Arc::new(SystemClock),
                    ids: Arc::new(DefaultIdGenerator::new()),
                });
                let storage = repo
                    .create_typed(&JsonlSessionCreateOptions {
                        id: Some("session".into()),
                        cwd: tmp.to_string_lossy().into_owned(),
                        parent_session_id: None,
                        metadata: None,
                    })
                    .await
                    .unwrap();
                let session = Session::new(Arc::new(storage.clone()), None);
                session
                    .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                        base: RecordBase {
                            id: "run".into(),
                            seq: 0,
                            lane: "main".into(),
                            timestamp: 0,
                        },
                        source_leaf_id: None,
                        intent: OperationIntent::Run {
                            original_prompt: Vec::new(),
                            initial_messages: Vec::new(),
                            system_prompt_override: None,
                            resume_data: None,
                        },
                    }))
                    .await
                    .unwrap();
                let recorder = crate::frame_progress::FrameRecordingEmitter::new(
                    Arc::new(rpi_agent::CollectorEmitter::default()),
                    session.clone(),
                    "main",
                    "run",
                );
                let mut partial = AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "m", 7);
                if tool_batch {
                    for id in ["call-a", "call-b"] {
                        partial
                            .content
                            .push(Content::ToolCall(rpi_ai::types::ToolCall {
                                kind: rpi_ai::types::ToolCallType,
                                id: id.into(),
                                name: "unavailable".into(),
                                arguments: serde_json::json!({}),
                                thought_signature: None,
                                namespace: None,
                                recorded: None,
                            }));
                    }
                    session
                        .view("main")
                        .append_message(AgentMessage::Assistant(Box::new(partial)))
                        .await
                        .unwrap();
                } else {
                    recorder
                        .emit(AgentEvent::MessageStart {
                            message_id: None,
                            message: AgentMessage::Assistant(Box::new(partial.clone())),
                        })
                        .await;
                    partial.content.push(Content::text("kept prefix"));
                    let partial = Arc::new(partial);
                    recorder
                        .emit(AgentEvent::MessageUpdate {
                            message_id: None,
                            message: partial.clone(),
                            assistant_message_event:
                                rpi_ai::types::AssistantMessageEvent::TextStart {
                                    content_index: 0,
                                    partial: partial.clone(),
                                },
                        })
                        .await;
                    recorder
                        .emit(AgentEvent::MessageUpdate {
                            message_id: None,
                            message: partial.clone(),
                            assistant_message_event:
                                rpi_ai::types::AssistantMessageEvent::TextDelta {
                                    content_index: 0,
                                    delta: "kept prefix".into(),
                                    partial,
                                },
                        })
                        .await;
                }
                recorder.flush().await.unwrap();
                let metadata = repo
                    .list_typed(&Default::default())
                    .await
                    .unwrap()
                    .remove(0);
                storage.install_append_handle(Box::new(FailAfter {
                    remaining: fail_after,
                    uncertain,
                    inner: fs.open_append_file(&metadata.path).await.unwrap().unwrap(),
                }));
                let error = AgentHarness::finish_interrupted_operation(
                    &session,
                    "main",
                    &[],
                    &CancellationToken::new(),
                    true,
                    None,
                )
                .await
                .expect_err("injected failure must propagate");
                assert!(error.to_string().contains("injected recovery failure"));
                assert_eq!(
                    session
                        .find_open_operations("main", None)
                        .await
                        .unwrap()
                        .len(),
                    1
                );
                drop(recorder);
                drop(session);
                drop(storage);
                let reopened = Session::new(
                    Arc::new(repo.open_by_jsonl_metadata(&metadata).await.unwrap()),
                    None,
                );
                for _ in 0..2 {
                    AgentHarness::finish_interrupted_operation(
                        &reopened,
                        "main",
                        &[],
                        &CancellationToken::new(),
                        true,
                        None,
                    )
                    .await
                    .unwrap();
                }
                let entries = reopened.find_entries(&EntryQuery::default()).await.unwrap();
                assert_eq!(entries.len(), if tool_batch { 3 } else { 1 },
                "recovery duplicates: tools={tool_batch}, uncertain={uncertain}, boundary={fail_after}");
                if tool_batch {
                    let results: std::collections::BTreeSet<_> = entries
                        .iter()
                        .filter_map(|entry| match entry {
                            Entry::Message(message) => match &message.message {
                                AgentMessage::ToolResult(result) => {
                                    Some(result.tool_call_id.as_str())
                                }
                                _ => None,
                            },
                            _ => None,
                        })
                        .collect();
                    assert_eq!(results, ["call-a", "call-b"].into_iter().collect());
                } else {
                    assert!(format!("{entries:?}").contains("kept prefix"));
                }
                assert!(reopened
                    .find_open_operations("main", None)
                    .await
                    .unwrap()
                    .is_empty());
                drop(reopened);
                std::fs::remove_dir_all(tmp).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn recovery_results_are_scoped_to_the_exact_assistant_batch() {
        let session = opts().session;
        let mut partial = AssistantMessage::empty(Api::Faux, "faux", "m", 0);
        partial.content.push(Content::ToolCall(
            serde_json::from_value(serde_json::json!({
                "id":"reused-call", "name":"echo", "arguments":{}
            }))
            .unwrap(),
        ));
        let result = crate::frame_progress::interrupted_tool_result(
            &crate::frame_progress::UnknownToolOutcome {
                tool_call_id: "reused-call".into(),
                tool_name: "echo".into(),
                arguments: serde_json::json!({}),
                recorded: None,
            },
            0,
        );
        for (id, message) in [
            ("old", AgentMessage::Assistant(Box::new(partial.clone()))),
            ("old-result", result.clone()),
            ("new", AgentMessage::Assistant(Box::new(partial.clone()))),
        ] {
            session
                .append_entry(
                    ProvisionedEntry {
                        id: id.into(),
                        kind: ProvisionedKind::Message {
                            message,
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
        }
        assert!(crate::frame_progress::unknown_tool_outcomes(
            &session,
            "main",
            Some("old"),
            &partial
        )
        .await
        .unwrap()
        .is_empty());
        assert_eq!(
            crate::frame_progress::unknown_tool_outcomes(&session, "main", Some("new"), &partial)
                .await
                .unwrap()
                .len(),
            1
        );
        session
            .append_entry(
                ProvisionedEntry {
                    id: "new-result".into(),
                    kind: ProvisionedKind::Message {
                        message: result,
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        assert!(crate::frame_progress::unknown_tool_outcomes(
            &session,
            "main",
            Some("new"),
            &partial
        )
        .await
        .unwrap()
        .is_empty());
        assert!(crate::frame_progress::unknown_tool_outcomes(
            &session,
            "main",
            Some("missing"),
            &partial
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn create_closes_interrupted_operation_when_restoring() {
        let options = opts();
        options
            .session
            .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                base: RecordBase {
                    id: "interrupted-run".into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                source_leaf_id: None,
                intent: OperationIntent::Run {
                    original_prompt: Vec::new(),
                    initial_messages: Vec::new(),
                    system_prompt_override: None,
                    resume_data: None,
                },
            }))
            .await
            .unwrap();

        let harness = AgentHarness::create(options).await.unwrap();

        assert!(harness
            .session()
            .find_open_operations("main", Some(1))
            .await
            .unwrap()
            .is_empty());
        let records = harness
            .session()
            .find_records(&RecordQuery {
                record_type: Some("operation_finished"),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(matches!(
            records.as_slice(),
            [LaneRecord::OperationFinished(finished)]
                if finished.run_id == "interrupted-run"
                    && finished.outcome == OperationOutcome::Aborted
        ));
    }

    /// `set_session` swaps the durable backing: the harness's `session()`
    /// facade (shared with lane handles) then reads the NEW storage's entries
    /// — the TUI `/session` hot-switch path.
    #[tokio::test]
    async fn set_session_swaps_durable_backing() {
        let h = AgentHarness::create(opts()).await.unwrap();
        // Build two JSONL sessions with one user message each.
        let tmp =
            std::env::temp_dir().join(format!("rpi-set-session-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cwd = tmp.to_string_lossy().into_owned();
        let env = Arc::new(OsExecutionEnv::with_cwd(tmp.clone()));
        let fs: Arc<dyn FileSystem> = env.clone();
        let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
            fs,
            sessions_root: tmp.join("sessions").to_string_lossy().into_owned(),
            clock: Arc::new(SystemClock),
            ids: Arc::new(DefaultIdGenerator::new()),
        });
        let s1 = repo
            .create_typed(&JsonlSessionCreateOptions {
                id: Some("sess-one".into()),
                parent_session_id: None,
                cwd: cwd.clone(),
                metadata: None,
            })
            .await
            .unwrap();
        let s2 = repo
            .create_typed(&JsonlSessionCreateOptions {
                id: Some("sess-two".into()),
                parent_session_id: None,
                cwd,
                metadata: None,
            })
            .await
            .unwrap();
        let sess1 = Session::new(Arc::new(s1), None);
        let sess2 = Session::new(Arc::new(s2), None);
        sess1
            .append_message(rpi_agent::AgentMessage::User(
                rpi_ai::types::UserMessage::new(String::from("alpha"), 1),
            ))
            .await
            .unwrap();
        sess2
            .append_message(rpi_agent::AgentMessage::User(
                rpi_ai::types::UserMessage::new(String::from("beta"), 2),
            ))
            .await
            .unwrap();

        h.set_session(sess1).await.unwrap();
        let e1 = h
            .session()
            .view("main")
            .find_entries(&EntryQuery::default())
            .await
            .unwrap();
        assert_eq!(e1.len(), 1);

        h.set_session(sess2).await.unwrap();
        let e2 = h
            .session()
            .view("main")
            .find_entries(&EntryQuery::default())
            .await
            .unwrap();
        assert_eq!(e2.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn non_main_lane_runner_persists_prompt_on_its_branch() {
        let h = AgentHarness::create(opts()).await.unwrap();
        h.session().create_lane("side", None).await.unwrap();
        let side = h.lane("side");

        let result = side.prompt_text("side branch", Vec::new()).await.unwrap();
        assert!(matches!(result.outcome, HarnessRunOutcome::Failed { .. }));

        let entries = side
            .session_view()
            .find_entries(&EntryQuery::default())
            .await
            .unwrap();
        assert!(entries.iter().any(|entry| {
            matches!(
                entry,
                Entry::Message(message)
                    if matches!(
                        &message.message,
                        AgentMessage::User(user)
                            if matches!(&user.content, UserContent::Text(text) if text == "side branch")
                    )
            )
        }));
    }
}

#[cfg(test)]
mod borrowed_context_tests {
    use super::*;
    use std::sync::Mutex;

    struct Probe(Arc<Mutex<(usize, Option<String>)>>);
    #[async_trait::async_trait]
    impl rpi_ai::Provider for Probe {
        fn id(&self) -> &str {
            "probe"
        }
        fn models(&self) -> &[Model] {
            &[]
        }
        async fn stream_simple(
            &self,
            model: &Model,
            context: &rpi_ai::Context,
            _: &rpi_ai::SimpleStreamOptions,
        ) -> rpi_ai::AssistantMessageEventStream {
            *self.0.lock().unwrap() = (
                context.messages.as_ptr() as usize,
                context.system_prompt.clone(),
            );
            let (mut producer, stream) =
                rpi_ai::event_stream::create_assistant_message_event_stream();
            producer
                .push(rpi_ai::AssistantMessageEvent::Done {
                    reason: rpi_ai::DoneReason::Stop,
                    message: AssistantMessage::empty(model.api.clone(), "probe", "m", 0),
                })
                .await;
            stream
        }
    }
    struct Hook(u8);
    impl rpi_ai::ProviderHooks for Hook {
        fn transform_context(
            &self,
            _: &Model,
            _: &rpi_ai::Context,
        ) -> Option<rpi_ai::ContextPatch> {
            match self.0 {
                0 => None,
                1 => Some(Default::default()),
                _ => Some(rpi_ai::ContextPatch {
                    system_prompt: Some("patched".into()),
                }),
            }
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observational_hooks_borrow_history_and_mutating_hooks_are_isolated() {
        for hook in [None, Some(0), Some(1), Some(2)] {
            let observed = Arc::new(Mutex::new((0, None)));
            let provider = Arc::new(Probe(Arc::clone(&observed)));
            let stream_fn = AgentHarness::build_stream_fn(
                vec![provider],
                hook.map(|kind| Arc::new(Hook(kind)) as Arc<dyn rpi_ai::ProviderHooks>),
                Default::default(),
            )
            .unwrap();
            let mut context =
                rpi_ai::Context::new(vec![
                    rpi_ai::UserMessage::new("history".repeat(4096), 0).into()
                ]);
            context.system_prompt = Some("original".into());
            let model = Model::new("m", "M", rpi_ai::Api::Faux, "probe", "http://localhost");
            stream_fn(&model, &context, &Default::default())
                .result()
                .await
                .unwrap();
            let (pointer, prompt) = observed.lock().unwrap().clone();
            if hook == Some(2) {
                assert_ne!(pointer, context.messages.as_ptr() as usize);
                assert_eq!(prompt.as_deref(), Some("patched"));
            } else {
                assert_eq!(pointer, context.messages.as_ptr() as usize);
                assert_eq!(prompt.as_deref(), Some("original"));
            }
            assert_eq!(context.system_prompt.as_deref(), Some("original"));
        }
    }
}

#[cfg(test)]
mod unsaved_isolation_tests {
    use super::*;
    use crate::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use crate::session::memory::{InMemorySessionStorage, SystemClock};
    use crate::session::session::DefaultIdGenerator;
    use crate::session::types::SessionMetadata;
    use rpi_tools::{FileSystem, OsExecutionEnv};

    fn metadata(id: &str) -> crate::session::types::SessionMetadata {
        SessionMetadata {
            id: id.into(),
            created_at: 0,
            parent_session_id: None,
        }
    }

    fn session(id: &str) -> Session {
        Session::new(
            Arc::new(InMemorySessionStorage::new(
                metadata(id),
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )),
            None,
        )
    }

    async fn harness_on(id: &str) -> AgentHarness {
        let mut options = AgentHarnessOptions::default();
        options.session = session(id);
        AgentHarness::create(options).await.unwrap()
    }

    /// Recovery copies are scoped by session and lane. The vector is
    /// harness-wide and outlives `set_session` switches, so an unscoped reader
    /// would export session A's refused content as part of session B — the
    /// isolation defect the maintainer reproduced through the real export path
    /// (the rpi-cli probe drives the actual `export_session`; this one pins the
    /// harness-side contract that makes it hold). Copies are retained, not
    /// cleared: returning to session A restores its recovery data.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_session_switch_does_not_expose_previous_session_recovery() {
        let harness = harness_on("session-a").await;
        harness.keep_unsaved_message_for_test(
            "a message you sent",
            crate::messages::create_custom_message(
                "probe",
                UserContent::Text("SESSION_A_SECRET".into()),
                true,
                None,
                0,
            ),
        );
        assert_eq!(harness.unsaved_messages().len(), 1);

        // Switch to an empty session B: its view must not contain A's copy.
        harness.set_session(session("session-b")).await.unwrap();
        let unsaved_b = harness.unsaved_messages();
        assert!(
            !format!("{unsaved_b:?}").contains("SESSION_A_SECRET"),
            "session B must not see session A's recovery data: {unsaved_b:?}"
        );

        // The data is retained for the session that owns it: switching back
        // must restore access, not discover the copy was discarded.
        harness.set_session(session("session-a")).await.unwrap();
        let unsaved_a = harness.unsaved_messages();
        assert!(
            format!("{unsaved_a:?}").contains("SESSION_A_SECRET"),
            "returning to session A must restore its recovery data: {unsaved_a:?}"
        );
    }

    /// A refused append on a non-main lane must not leak into the main lane's
    /// recovery view: the same session-level isolation applies per lane, since
    /// `export_session` reads only the lane it is called with.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_unsaved_copies_are_scoped_per_lane_within_a_session() {
        let harness = harness_on("session-a").await;
        harness.lane("side"); // registers nothing durable; only main exists
        harness.keep_unsaved_message_for_test(
            "a message you sent",
            crate::messages::create_custom_message(
                "probe",
                UserContent::Text("SIDE_LANE_ONLY".into()),
                true,
                None,
                0,
            ),
        );
        // The copy was stamped on the harness's own lane ("main"). A main-lane
        // reader must see it; this pins the stamp being recorded at all.
        let unsaved = harness.unsaved_messages();
        assert!(
            format!("{unsaved:?}").contains("SIDE_LANE_ONLY"),
            "a copy stamped for the current session+lane must be visible: {unsaved:?}"
        );
    }

    /// The maintainer's P2: a refused write on the normal prompt path (the
    /// operation_started+first-entry append) must keep a recovery copy, not
    /// only the out-of-band `append_message` wrapper. Driven through a real
    /// JSONL backend whose appends all fail, exactly the way a full disk does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_refused_normal_prompt_has_a_recovery_copy() {
        /// Refuses every append, the way a full disk does.
        struct RefusingAppend;

        #[async_trait::async_trait]
        impl rpi_tools::env::AppendFile for RefusingAppend {
            async fn append(&mut self, _: &[u8]) -> Result<(), rpi_tools::FileError> {
                Err(rpi_tools::FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "disk full",
                ))
            }
        }

        let tmp =
            std::env::temp_dir().join(format!("rpi-normal-prompt-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let env = Arc::new(OsExecutionEnv::with_cwd(tmp.clone()));
        let fs: Arc<dyn FileSystem> = env.clone();
        let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
            fs,
            sessions_root: tmp.join("sessions").to_string_lossy().into_owned(),
            clock: Arc::new(SystemClock),
            ids: Arc::new(DefaultIdGenerator::new()),
        });
        let storage = repo
            .create_typed(&JsonlSessionCreateOptions {
                id: Some("sess-write".into()),
                parent_session_id: None,
                cwd: tmp.to_string_lossy().into_owned(),
                metadata: None,
            })
            .await
            .unwrap();
        storage.install_append_handle(Box::new(RefusingAppend));

        let mut options = AgentHarnessOptions::default();
        options.session = Session::new(Arc::new(storage), None);
        let harness = AgentHarness::create(options).await.unwrap();
        let lane = harness.lane("main");

        let result = lane.prompt_text("UNSAVED_NORMAL_PROMPT", vec![]).await;
        let Err(error) = result else {
            panic!("the disk must refuse the prompt write")
        };
        assert!(
            error.to_string().contains("disk full"),
            "unexpected failure instead of a refused write: {error}"
        );
        let unsaved = harness.unsaved_messages();
        assert!(
            format!("{unsaved:?}").contains("UNSAVED_NORMAL_PROMPT"),
            "a normal failed prompt needs the recovery copy promised by the warning: {unsaved:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod agent_end_log_barrier_tests {
    use super::*;
    use rpi_ai::providers::faux::{FauxProvider, FauxScript};

    /// `AgentEnd` is the log-completion barrier for the broadcast: when it is
    /// forwarded, every message of the finished run — including the final
    /// assistant message, which the loop emits `MessageEnd` for before
    /// `run_core` persists it — must already be in the durable log. The probe
    /// wraps the emitter and inspects the log exactly when `AgentEnd` reaches
    /// the forwarding point, the same moment the TUI's footer reconciliation
    /// would read it.
    struct ProbeBarrierEmitter {
        inner: Arc<dyn AgentEmitter>,
        harness: Mutex<Option<AgentHarness>>,
        seen_final_at_barrier: Arc<Mutex<bool>>,
    }
    impl AgentEmitter for ProbeBarrierEmitter {
        fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                let harness = self.harness.lock().unwrap().clone();
                if let Some(harness) = harness {
                    let entries = futures::executor::block_on(async {
                        harness
                            .session()
                            .view("main")
                            .find_entries(&crate::session::types::EntryQuery {
                                order: Some(crate::session::types::EntryOrder::OldestFirst),
                                ..Default::default()
                            })
                            .await
                            .unwrap_or_default()
                    });
                    let has_assistant = entries.iter().any(|entry| {
                        matches!(
                            entry,
                            crate::session::types::Entry::Message(me)
                                if matches!(me.message, rpi_agent::AgentMessage::Assistant(_))
                        )
                    });
                    *self.seen_final_at_barrier.lock().unwrap() = has_assistant;
                }
            }
            self.inner.emit(event)
        }
        fn try_emit(&self, event: AgentEvent) {
            self.inner.try_emit(event);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_agent_end_is_forwarded_only_after_the_final_turn_is_durable() {
        let provider: Arc<dyn rpi_ai::Provider> =
            FauxProvider::new(FauxScript::new().with_text("final reply"));
        let base_emitter: Arc<dyn AgentEmitter> = Arc::new(rpi_agent::CollectorEmitter::default());
        let seen_final_at_barrier = Arc::new(Mutex::new(false));
        let options = AgentHarnessOptions {
            model: FauxProvider::new(FauxScript::new()).default_model().clone(),
            models: vec![Arc::clone(&provider)],
            session: crate::session::session::Session::new(
                Arc::new(crate::session::memory::InMemorySessionStorage::new(
                    crate::session::types::SessionMetadata {
                        id: "log-barrier".into(),
                        created_at: 0,
                        parent_session_id: None,
                    },
                    Arc::new(crate::session::memory::SystemClock),
                    Arc::new(crate::session::session::DefaultIdGenerator::new()),
                )),
                None,
            ),
            ..Default::default()
        };
        let harness = AgentHarness::create(options).await.unwrap();
        let emitter = Arc::new(ProbeBarrierEmitter {
            inner: base_emitter,
            harness: Mutex::new(Some(harness.clone())),
            seen_final_at_barrier: Arc::clone(&seen_final_at_barrier),
        });
        harness.set_agent_emitter(Some(emitter)).await.unwrap();
        let lane = harness.lane("main");

        lane.prompt_text("one prompt", vec![]).await.unwrap();

        assert!(
            *seen_final_at_barrier.lock().unwrap(),
            "when AgentEnd is forwarded, the run's final assistant message must \
             already be in the durable log (the footer reconciles from the log \
             exactly there)"
        );
    }
}

#[cfg(test)]
mod recorded_recovery_tests {
    use super::*;
    use crate::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
    use crate::session::session::Session;
    use crate::session::types::{SessionMetadata, ToolStartedRecord};
    use crate::types::ToolReplay as HarnessReplay;
    use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
    use rpi_ai::types::{Api, Content, Tool};

    /// A replay-safe tool whose `execute` is observable through `executions`.
    struct CountingTool {
        schema: Tool,
        executions: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl AgentTool for CountingTool {
        fn schema(&self) -> &Tool {
            &self.schema
        }
        fn label(&self) -> &str {
            "Shell"
        }
        async fn execute(
            &self,
            _id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
        ) -> Result<AgentToolResult, AgentError> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("executed locally"))
        }
    }

    #[tokio::test]
    async fn recovery_does_not_bypass_an_installed_authorization_hook() {
        let executions = Arc::new(AtomicUsize::new(0));
        let options = AgentHarnessOptions {
            tools: shell_tool(&executions),
            before_tool_call: Some(Arc::new(|_, _| Box::pin(async { None }))),
            ..Default::default()
        };
        let harness = AgentHarness::create(options).await.unwrap();
        assert_eq!(harness.get_tools().await.unwrap().len(), 1);
        assert!(
            harness.recovery_tools().is_empty(),
            "startup recovery must not replay without authorization context"
        );
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    }

    fn shell_tool(executions: &Arc<AtomicUsize>) -> Vec<HarnessTool> {
        let schema = Tool {
            name: "shell".into(),
            description: "Run a shell command".into(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "additionalProperties": false
            })),
            constrained_sampling: None,
        };
        let tool = CountingTool {
            schema,
            executions: Arc::clone(executions),
        };
        vec![HarnessTool::new(Arc::new(tool)).with_replay(HarnessReplay::Safe)]
    }

    fn safe_record(call_id: &str) -> BTreeMap<String, ToolStartedRecord> {
        BTreeMap::from([(
            call_id.to_string(),
            ToolStartedRecord {
                base: RecordBase {
                    id: "tool-started-1".into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                run_id: "run".into(),
                assistant_entry_id: "assistant".into(),
                tool_index: 0,
                tool_call_id: call_id.into(),
                tool_name: "shell".into(),
                effective_args: serde_json::json!({ "command": "rm -rf target" }),
                result_entry_id: "result".into(),
                replay: ToolReplay::Safe,
            },
        )])
    }

    fn empty_session() -> Session {
        Session::new(
            Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: "memory".into(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(FakeClock::new()),
                Arc::new(CounterIdGenerator::new()),
            )),
            None,
        )
    }

    fn partial_with(call: serde_json::Value) -> AssistantMessage {
        let mut partial = AssistantMessage::empty(Api::Faux, "external", "model", 7);
        partial
            .content
            .push(serde_json::from_value::<Content>(call).expect("tool call decodes"));
        partial
    }

    #[tokio::test]
    async fn recovery_repairs_a_partial_tool_batch_behind_extension_entries() {
        let session = empty_session();
        let mut partial = partial_with(serde_json::json!({
            "type": "toolCall", "id": "call-1", "name": "shell", "arguments": {}
        }));
        partial.content.push(
            serde_json::from_value(serde_json::json!({
                "type": "toolCall", "id": "call-2", "name": "shell", "arguments": {}
            }))
            .unwrap(),
        );
        session
            .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                base: RecordBase {
                    id: "run".into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                source_leaf_id: None,
                intent: OperationIntent::Run {
                    original_prompt: Vec::new(),
                    initial_messages: Vec::new(),
                    system_prompt_override: None,
                    resume_data: None,
                },
            }))
            .await
            .unwrap();
        session
            .append_entry(
                ProvisionedEntry {
                    id: "assistant".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::Assistant(Box::new(partial)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        let outcome = crate::frame_progress::UnknownToolOutcome {
            tool_call_id: "call-1".into(),
            tool_name: "shell".into(),
            arguments: serde_json::json!({}),
            recorded: None,
        };
        session
            .append_message(crate::frame_progress::interrupted_tool_result(&outcome, 7))
            .await
            .unwrap();
        session
            .append_custom_entry("durable", Some(serde_json::json!({"revision": 1})))
            .await
            .unwrap();

        AgentHarness::finish_interrupted_operation(
            &session,
            "main",
            &[],
            &CancellationToken::new(),
            true,
            None,
        )
        .await
        .unwrap();
        let entries = session.find_entries(&EntryQuery::default()).await.unwrap();
        let calls: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Message(message) => match &message.message {
                    AgentMessage::ToolResult(result) => Some(result.tool_call_id.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(calls.iter().filter(|id| **id == "call-1").count(), 1);
        assert_eq!(calls.iter().filter(|id| **id == "call-2").count(), 1);
        assert!(session
            .find_open_operations("main", None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn recovery_tip_checks_ignore_extension_state_entries() {
        let session = empty_session();
        session
            .append_message(AgentMessage::User(UserMessage::new("question", 0)))
            .await
            .unwrap();
        session
            .append_custom_entry("todo", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert!(AgentHarness::tip_is_user_message(&session, "main").await);
        session
            .append_message(AgentMessage::Assistant(Box::new(AssistantMessage::empty(
                Api::Faux,
                "faux",
                "model",
                7,
            ))))
            .await
            .unwrap();
        session
            .append_custom_entry("goal", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert!(AgentHarness::tip_is_assistant(&session, "main").await);
    }

    /// Crash recovery must not re-run a call that already ran upstream, even
    /// when the tool is replay-safe and the call was recorded as safe to replay.
    /// The recording is the outcome, so it is reported rather than repeated.
    #[tokio::test]
    async fn recovery_answers_a_recorded_call_without_replaying_it() {
        let executions = Arc::new(AtomicUsize::new(0));
        let tools = shell_tool(&executions);
        let partial = partial_with(serde_json::json!({
            "type": "toolCall",
            "id": "call-1",
            "name": "shell",
            "arguments": { "command": "rm -rf target" },
            "recorded": {
                "version": 1,
                "isError": false,
                "result": {
                    "content": [{ "type": "text", "text": "recorded output" }],
                    "details": {},
                    "terminate": true
                }
            }
        }));
        let outcomes =
            crate::frame_progress::unknown_tool_outcomes(&empty_session(), "main", None, &partial)
                .await
                .expect("outcomes");
        assert_eq!(outcomes.len(), 1);

        let message = AgentHarness::resolve_tool_call(
            &tools,
            &CancellationToken::new(),
            &safe_record("call-1"),
            &outcomes[0],
            7,
        )
        .await
        .unwrap();

        assert_eq!(
            executions.load(Ordering::SeqCst),
            0,
            "a recorded call is never replayed"
        );
        let AgentMessage::ToolResult(result) = message else {
            panic!("expected a tool result");
        };
        assert!(!result.is_error);
        let text: String = result
            .content
            .iter()
            .filter_map(|block| match block {
                Content::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "recorded output");
    }

    /// The ordinary replay path is unchanged for calls without a recording.
    #[tokio::test]
    async fn recovery_still_replays_an_ordinary_safe_call() {
        let executions = Arc::new(AtomicUsize::new(0));
        let tools = shell_tool(&executions);
        let partial = partial_with(serde_json::json!({
            "type": "toolCall",
            "id": "call-1",
            "name": "shell",
            "arguments": { "command": "rm -rf target" }
        }));
        let outcomes =
            crate::frame_progress::unknown_tool_outcomes(&empty_session(), "main", None, &partial)
                .await
                .expect("outcomes");

        let _ = AgentHarness::resolve_tool_call(
            &tools,
            &CancellationToken::new(),
            &safe_record("call-1"),
            &outcomes[0],
            7,
        )
        .await
        .unwrap();

        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_cancelled_recovery_cannot_enter_an_ordinary_safe_tool() {
        let executions = Arc::new(AtomicUsize::new(0));
        let tools = shell_tool(&executions);
        let outcome = crate::frame_progress::UnknownToolOutcome {
            tool_call_id: "call-1".into(),
            tool_name: "shell".into(),
            arguments: serde_json::json!({"command":"side effect"}),
            recorded: None,
        };
        let signal = CancellationToken::new();
        signal.cancel();
        assert!(AgentHarness::resolve_tool_call(
            &tools,
            &signal,
            &safe_record("call-1"),
            &outcome,
            7,
        )
        .await
        .is_err());
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    }
}
