//! Commit-on-settle: persist a run's messages when they settle, not at run end.
//!
//! # Why this exists
//!
//! `tool_started` is the record that lets recovery say *which tool's side effect
//! is unknown* after a crash. It cannot be written from rpi's old model, where a
//! whole run's messages were persisted in one pass at the end: the reducer's
//! `validate_tool_start` requires `assistant_entry_id` to **already exist** with
//! a matching tool call at `tool_index`, and a tool runs long before the run
//! finishes. Upstream satisfies that requirement by committing the assistant
//! message when the step settles (`assistant.ready`), *then* running its tools.
//!
//! This module ports that ordering: an assistant message carrying tool calls is
//! committed as soon as it is final (`MessageEnd`), with a **reserved** entry id
//! that the matching `step_attempt` record names. Once the message is an entry,
//! its tools can be recorded before they run.
//!
//! # What is deliberately *not* committed early
//!
//! - **The final assistant message** (no tool calls): it ends the run and is
//!   persisted by the normal run-end pass, so `leaf_id`/`final_entry_id` and the
//!   `Completed` outcome keep their existing meaning.
//! - **A message from an attempt that will be retried**: the harness re-runs the
//!   whole loop on a retryable failure, so committing a failed attempt's message
//!   would leave a dead assistant message in history and put two assistant turns
//!   in a row in front of the provider. The retry decision is computable at
//!   settle time (`enabled && attempt < max && is_retryable_error`), and is
//!   evaluated here with the same inputs the retry loop uses.
//!
//! # Interaction with the compaction cut
//!
//! A message committed here is already an entry, so the run-end pass cannot
//! "skip" it via `post_compaction_cut`. That means messages summarized by a
//! mid-run compaction stay in the log. That matches upstream (its compaction
//! commits a summary entry and leaves the pre-summary entries in place; context
//! building starts after the last compaction via the cut-point scan) and is the
//! reason this is a deliberate trade rather than an oversight.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use futures::future::BoxFuture;
use rpi_agent::{AgentEmitter, AgentEvent, AgentMessage};
use rpi_ai::types::{AssistantMessage, StopReason};

use crate::agent_harness::{AgentHarness, QueueReservation};
use crate::session::types::{
    ProvisionedEntry, ProvisionedKind, RecordBase, ToolReplay, ToolStartedRecord,
};

static SETTLE_DIAGNOSTICS: OnceLock<bool> = OnceLock::new();

fn settle_diagnostics_enabled() -> bool {
    *SETTLE_DIAGNOSTICS
        .get_or_init(|| std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0"))
}

#[derive(Clone, Copy)]
enum SettlePhase {
    Emit,
    EmitHandle,
    SettleAwait,
    InnerEmit,
    PrepareCommit,
    AssistantEntryStep,
    ClearStream,
    ToolStarted,
    ToolResultEntry,
}

struct SettleDiagnostics {
    enabled: bool,
    counts: [AtomicU64; 9],
    elapsed_ns: [AtomicU64; 9],
}

impl SettleDiagnostics {
    fn new() -> Self {
        Self {
            enabled: settle_diagnostics_enabled(),
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            elapsed_ns: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn index(phase: SettlePhase) -> usize {
        match phase {
            SettlePhase::Emit => 0,
            SettlePhase::EmitHandle => 1,
            SettlePhase::SettleAwait => 2,
            SettlePhase::InnerEmit => 3,
            SettlePhase::PrepareCommit => 4,
            SettlePhase::AssistantEntryStep => 5,
            SettlePhase::ClearStream => 6,
            SettlePhase::ToolStarted => 7,
            SettlePhase::ToolResultEntry => 8,
        }
    }

    fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    fn record(&self, phase: SettlePhase, started: Option<Instant>) {
        if let Some(started) = started {
            let index = Self::index(phase);
            self.counts[index].fetch_add(1, Ordering::Relaxed);
            self.elapsed_ns[index].fetch_add(
                started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
    }

    fn report(&self, run_id: &str) {
        if !self.enabled {
            return;
        }
        let names = [
            "emit",
            "emit_handle",
            "settle_await",
            "inner_emit",
            "prepare_commit",
            "assistant_entry_step",
            "clear_stream",
            "tool_started",
            "tool_result_entry",
        ];
        let mut fields = String::new();
        for (index, name) in names.iter().enumerate() {
            use std::fmt::Write;
            let _ = write!(
                fields,
                " {}_calls={} {}_ns={}",
                name,
                self.counts[index].load(Ordering::Relaxed),
                name,
                self.elapsed_ns[index].load(Ordering::Relaxed),
            );
        }
        eprintln!("RPI_SETTLE_DIAGNOSTICS run_id={}{}", run_id, fields);
    }
}

pub fn will_retry(
    message: &AssistantMessage,
    retry_enabled: bool,
    attempt: u32,
    max_retries: u32,
) -> bool {
    retry_enabled
        && attempt < max_retries
        && message.stop_reason == StopReason::Error
        && crate::compaction::is_retryable_assistant_error(message)
}

/// Whether a settled assistant message must be committed early.
///
/// Only messages that lead into tool execution need to be entries before the run
/// ends, because only they are referenced by `tool_started` records.
fn needs_early_commit(message: &AssistantMessage, retrying: bool) -> bool {
    !retrying
        && message
            .content
            .iter()
            .any(|block| matches!(block, rpi_ai::types::Content::ToolCall(_)))
}

/// A refused final entry ACK is retryable, but its AgentEnd is not observable
/// until the ordered run-end persistence pass succeeds. This is explicit state:
/// message/commit counts do not describe persistence across compaction cuts.
enum FinalAssistantBarrier {
    Ready,
    AwaitingPersistence,
    Pending(Box<AgentEvent>),
}

/// What a settle observer needs to know about the run it is watching. Shared
/// with the run loop, which owns the retry counter.
pub struct SettleState {
    /// Index of the next loop message, in `new_messages` order. Advanced by
    /// `MessageStart` so a commit can name the index it persisted.
    seen: AtomicUsize,
    /// The attempt number the retry loop is currently on.
    attempt: AtomicU32,
    retry_enabled: bool,
    max_retries: u32,
    run_id: String,
    /// Per-tool replay policy, from the run's tool snapshot. A `ToolStarted`
    /// record has to carry it, and the settle observer sees only the event.
    tool_replays: BTreeMap<String, ToolReplay>,
    /// How many assistant messages have settled. The n-th settled assistant
    /// message is the recorder's stream n: both are assigned in event order, and
    /// only assistant messages create streams.
    assistant_settled: AtomicUsize,
    /// Indices already committed. Every commit path checks its predecessors,
    /// so this is a contiguous prefix of the current attempt's message indices.
    committed: Mutex<BTreeSet<usize>>,
    /// The entry each settled message produced, by loop-message index.
    assistant_entries: Mutex<BTreeMap<usize, String>>,
    /// Loop-message index to its run-wide frame stream. Used by every assistant
    /// commit path, including context boundaries and the final persistence pass.
    assistant_streams: Mutex<BTreeMap<usize, usize>>,
    /// Stable commit reservations, including externally queued messages and
    /// the first generated assistant of an accepted retry. Live events and every
    /// persistence fallback reuse this reservation; publishing never commits it.
    reserved_entries: Mutex<BTreeMap<usize, String>>,
    injected_indices: Mutex<BTreeSet<usize>>,
    retry_result: Mutex<Option<String>>,
    /// `tool_call_id` → (assistant entry id, ordinal within that message). Filled
    /// when a message carrying tool calls settles, read when each tool starts.
    pending_calls: Mutex<BTreeMap<String, (String, u32)>>,
    /// `tool_call_id` → reserved result entry id, so the tool result is committed
    /// under the id its `tool_started` record already named.
    reserved_results: Mutex<BTreeMap<String, String>>,
    /// Queued messages handed to the loop for injection, in the order they will be
    /// emitted. MessageStart transfers each reservation to its loop-message
    /// index, so every role can commit at settle or fall back to run-end writes
    /// without changing the durable queue target's id.
    injected: Mutex<VecDeque<String>>,
    /// Run-local reservations retain uncommitted inputs until their entry ACK.
    injected_reservations: Mutex<Vec<QueueReservation>>,
    /// A final assistant normally waits for run-end persistence. If a queued
    /// message continues this run, commit that assistant before the injection.
    uncommitted_assistant: Mutex<Option<(usize, usize, AssistantMessage)>>,
    final_assistant_barrier: Mutex<FinalAssistantBarrier>,
    /// The durable frame writer, installed after the run's emitter is created.
    /// Commit paths flush it before binding an assistant entry.
    frame_recorder: Mutex<Option<Arc<crate::frame_progress::FrameRecordingEmitter>>>,
    identity_error: Mutex<Option<String>>,
    diagnostics: SettleDiagnostics,
}

impl SettleState {
    pub fn new(
        run_id: impl Into<String>,
        retry_enabled: bool,
        max_retries: u32,
        tool_replays: BTreeMap<String, ToolReplay>,
    ) -> Self {
        Self {
            seen: AtomicUsize::new(0),
            attempt: AtomicU32::new(0),
            retry_enabled,
            max_retries,
            run_id: run_id.into(),
            tool_replays,
            assistant_settled: AtomicUsize::new(0),
            committed: Mutex::new(BTreeSet::new()),
            assistant_entries: Mutex::new(BTreeMap::new()),
            assistant_streams: Mutex::new(BTreeMap::new()),
            reserved_entries: Mutex::new(BTreeMap::new()),
            injected_indices: Mutex::new(BTreeSet::new()),
            retry_result: Mutex::new(None),
            pending_calls: Mutex::new(BTreeMap::new()),
            reserved_results: Mutex::new(BTreeMap::new()),
            injected: Mutex::new(VecDeque::new()),
            injected_reservations: Mutex::new(Vec::new()),
            uncommitted_assistant: Mutex::new(None),
            final_assistant_barrier: Mutex::new(FinalAssistantBarrier::Ready),
            frame_recorder: Mutex::new(None),
            identity_error: Mutex::new(None),
            diagnostics: SettleDiagnostics::new(),
        }
    }

    pub(crate) fn identity_error(&self) -> Option<String> {
        self.identity_error.lock().unwrap().clone()
    }

    pub(crate) fn set_frame_recorder(
        &self,
        recorder: Arc<crate::frame_progress::FrameRecordingEmitter>,
    ) {
        *self.frame_recorder.lock().unwrap() = Some(recorder);
    }

    async fn flush_frame_progress(&self) -> crate::result::HarnessResult<()> {
        let recorder = self.frame_recorder.lock().unwrap().clone();
        if let Some(recorder) = recorder {
            recorder
                .flush_with_reason(crate::frame_progress::FrameFlushReason::PrepareCommit)
                .await?;
        }
        Ok(())
    }

    pub(crate) fn hold_injected_reservation(&self, reservation: QueueReservation) {
        self.injected_reservations.lock().unwrap().push(reservation);
    }

    /// Register a message the run is about to inject, with the entry id its
    /// `queue_enqueued` record reserved for it.
    pub fn push_injected(&self, entry_id: impl Into<String>, _message: AgentMessage) {
        self.injected.lock().unwrap().push_back(entry_id.into());
    }

    /// Start an attempt after its retry wait. Resetting before the wait would
    /// lose the previous attempt's committed indices if the wait is cancelled.
    pub fn set_attempt(&self, attempt: u32) {
        self.attempt.store(attempt, Ordering::Release);
        // Each invocation of the loop returns its own new_messages indices.
        // Frame stream ordinals, however, remain run-wide across retries.
        self.seen.store(0, Ordering::Release);
        self.committed.lock().unwrap().clear();
        self.assistant_entries.lock().unwrap().clear();
        self.assistant_streams.lock().unwrap().clear();
        self.reserved_entries.lock().unwrap().clear();
        self.injected_indices.lock().unwrap().clear();
        self.retry_result.lock().unwrap().take();
        self.uncommitted_assistant.lock().unwrap().take();
        *self.final_assistant_barrier.lock().unwrap() = FinalAssistantBarrier::Ready;
    }

    /// Called only after every new message in this attempt has been persisted.
    /// Bypass SettlingEmitter: replaying handle would mutate message/stream state.
    /// The original recorder still forwards the event to the original consumer.
    pub(crate) async fn release_pending_agent_end(&self, emitter: &dyn AgentEmitter) {
        if self.identity_error().is_some() {
            return;
        }
        let pending = std::mem::replace(
            &mut *self.final_assistant_barrier.lock().unwrap(),
            FinalAssistantBarrier::Ready,
        );
        if let FinalAssistantBarrier::Pending(event) = pending {
            emitter.emit(*event).await;
        }
    }

    pub(crate) fn reserve_retry_result(&self, entry_id: String) {
        *self.retry_result.lock().unwrap() = Some(entry_id);
    }

    pub(crate) fn stream_at(&self, index: usize) -> Option<usize> {
        self.assistant_streams.lock().unwrap().get(&index).copied()
    }

    /// Reuse reservations on fallback writes as well as the normal settle path.
    pub(crate) fn reserve_entry_id(&self, index: usize, harness: &AgentHarness) -> String {
        self.reserved_entries
            .lock()
            .unwrap()
            .entry(index)
            .or_insert_with(|| harness.next_entry_id())
            .clone()
    }

    /// Indices already persisted by this observer.
    pub fn committed_indices(&self) -> BTreeSet<usize> {
        self.committed.lock().unwrap().clone()
    }

    pub fn committed_count(&self) -> usize {
        self.committed.lock().unwrap().len()
    }

    fn ensure_commit_prefix(&self, index: usize) -> crate::result::HarnessResult<()> {
        let committed = self.committed_count();
        if committed < index {
            return Err(crate::result::HarnessError::io(format!(
                "cannot commit message {index} before pending message {committed}"
            )));
        }
        Ok(())
    }

    /// The prefix a turn boundary must make durable before continuing. Keep the
    /// final/retryable assistant deferred as before; earlier messages (including
    /// every tool result) must be entries before compaction or another request.
    pub(crate) fn required_prefix_len(&self, messages: &[AgentMessage]) -> usize {
        match messages.last() {
            Some(AgentMessage::Assistant(message))
                if !needs_early_commit(
                    message,
                    will_retry(
                        message,
                        self.retry_enabled,
                        self.attempt(),
                        self.max_retries,
                    ),
                ) =>
            {
                messages.len() - 1
            }
            _ => messages.len(),
        }
    }

    pub(crate) fn attempt(&self) -> u32 {
        self.attempt.load(Ordering::Acquire)
    }

    pub(crate) fn entry_at(&self, index: usize) -> Option<String> {
        self.assistant_entries.lock().unwrap().get(&index).cloned()
    }

    async fn flush_prepare_commit(&self) -> crate::result::HarnessResult<()> {
        let prepare_started = self.diagnostics.start();
        let prepare_result = self.flush_frame_progress().await;
        self.diagnostics
            .record(SettlePhase::PrepareCommit, prepare_started);
        prepare_result
    }

    /// Flush frame progress before a commit whose binding is written together
    /// with the entry. The binding itself is appended by the caller so it can
    /// share the entry/step durable append.
    pub(crate) async fn prepare_commit_for_assistant(
        &self,
        index: usize,
    ) -> crate::result::HarnessResult<()> {
        self.ensure_commit_prefix(index)?;
        self.flush_prepare_commit().await
    }

    /// Make assistant persistence idempotent across a crash between the entry
    /// write and frame retirement. Non-assistant indices have no stream.
    pub(crate) async fn prepare_commit(
        &self,
        index: usize,
        entry_id: &str,
        harness: &AgentHarness,
    ) -> crate::result::HarnessResult<()> {
        self.ensure_commit_prefix(index)?;
        self.flush_prepare_commit().await?;
        let stream_index = self.assistant_streams.lock().unwrap().get(&index).copied();
        if let Some(stream_index) = stream_index {
            crate::frame_progress::bind_stream_entry(
                harness.session(),
                harness.lane_name(),
                &self.run_id,
                stream_index,
                entry_id,
            )
            .await?;
        }
        Ok(())
    }

    pub(crate) fn record_commit(&self, index: usize, entry_id: &str) {
        self.committed.lock().unwrap().insert(index);
        self.assistant_entries
            .lock()
            .unwrap()
            .insert(index, entry_id.to_string());
    }

    /// Persist actual execution intent after argument preparation/authorization.
    /// The caller must not enter the underlying tool unless this acknowledges.
    pub(crate) async fn start_tool(
        &self,
        harness: &AgentHarness,
        tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> crate::result::HarnessResult<()> {
        let (assistant_entry_id, tool_index) = self
            .pending_calls
            .lock()
            .unwrap()
            .get(tool_call_id)
            .cloned()
            .ok_or_else(|| {
                crate::result::HarnessError::io(format!(
                    "cannot execute tool {tool_call_id}: its assistant entry was not committed"
                ))
            })?;
        let result_entry_id = harness.next_entry_id();
        let record = ToolStartedRecord {
            base: RecordBase {
                id: harness.next_entry_id(),
                seq: 0,
                lane: harness.lane_name().to_string(),
                timestamp: 0,
            },
            run_id: self.run_id.clone(),
            assistant_entry_id,
            tool_index,
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            effective_args: args.clone(),
            result_entry_id: result_entry_id.clone(),
            replay: self
                .tool_replays
                .get(tool_name)
                .copied()
                .unwrap_or(ToolReplay::Never),
        };
        let started = self.diagnostics.start();
        let result = harness.write_tool_started(record).await;
        self.diagnostics.record(SettlePhase::ToolStarted, started);
        result?;
        self.reserved_results
            .lock()
            .unwrap()
            .insert(tool_call_id.to_string(), result_entry_id);
        Ok(())
    }

    /// Resolve a child against the real checkpointed root and committed assistant.
    /// A private nested request ID is not an assistant tool-call ID or authority.
    async fn child_origin(
        &self,
        session: &crate::session::Session,
        lane: &str,
        child: &rpi_agent::tool_program_observation::ToolProgramChild,
    ) -> Result<crate::tool_program_journal::ChildOrigin, String> {
        if self
            .tool_replays
            .get(&child.root_name)
            .copied()
            .unwrap_or(ToolReplay::Never)
            != ToolReplay::Never
            || !self
                .reserved_results
                .lock()
                .unwrap()
                .contains_key(&child.root_id)
        {
            return Err("child requires a checkpointed Never-replay root".into());
        }
        let (assistant_entry_id, tool_index) = self
            .pending_calls
            .lock()
            .unwrap()
            .get(&child.root_id)
            .cloned()
            .ok_or_else(|| "child root has no committed assistant slot".to_owned())?;
        let entries = session
            .view(lane)
            .find_entries(&crate::session::types::EntryQuery {
                entry_id: Some(assistant_entry_id.clone()),
                limit: Some(1),
                ..Default::default()
            })
            .await
            .map_err(|_| "child assistant lookup failed".to_owned())?;
        let Some(crate::session::types::Entry::Message(entry)) = entries.first() else {
            return Err("child assistant entry unavailable".into());
        };
        let AgentMessage::Assistant(message) = &entry.message else {
            return Err("child root does not belong to an assistant".into());
        };
        let call = message
            .content
            .iter()
            .filter_map(|content| match content {
                rpi_ai::types::Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .nth(tool_index as usize);
        if !call.is_some_and(|call| call.id == child.root_id && call.name == child.root_name) {
            return Err("child root differs from committed assistant slot".into());
        }
        Ok(crate::tool_program_journal::ChildOrigin {
            run_id: self.run_id.clone(),
            assistant_entry_id,
            tool_index,
        })
    }

    /// Nested effects are descendants of a real committed Never-replay call.
    /// Custom records are audit metadata, not fabricated assistant/tool messages.
    pub(crate) async fn start_nested_tool(
        &self,
        harness: &AgentHarness,
        parent: &rpi_agent::tool_program::NestedParent,
        id: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> crate::result::HarnessResult<()> {
        if self
            .tool_replays
            .get(&parent.root_name)
            .copied()
            .unwrap_or(ToolReplay::Never)
            != ToolReplay::Never
            || !self
                .reserved_results
                .lock()
                .unwrap()
                .contains_key(&parent.root_id)
        {
            return Err(crate::result::HarnessError::io(
                "nested invocation requires a checkpointed Never-replay parent",
            ));
        }
        harness.append_custom_entry("rpi.tool_program.intent.v1", Some(serde_json::json!({"runId":self.run_id,"rootToolCallId":parent.root_id,"parentToolCallId":parent.parent_id,"requestId":id,"name":name,"arguments":args,"replay":"never"}))).await?;
        Ok(())
    }

    /// Remember each tool call of a freshly committed assistant message, keyed so
    /// the execution barrier can record it against the right entry and ordinal.
    pub(crate) fn record_tool_calls(&self, entry_id: &str, message: &AssistantMessage) {
        let mut pending = self.pending_calls.lock().unwrap();
        // ToolStarted uses the reducer's tool-call-only ordinal, not the
        // position of the block among interleaved text/thinking content.
        for (ordinal, call) in message
            .content
            .iter()
            .filter_map(|block| match block {
                rpi_ai::types::Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .enumerate()
        {
            pending.insert(call.id.clone(), (entry_id.to_string(), ordinal as u32));
        }
    }
}

impl Drop for SettleState {
    fn drop(&mut self) {
        self.diagnostics.report(&self.run_id);
    }
}

/// Emitter wrapper that commits settled messages to the session.
///
/// Sits *around* the frame recorder, so frames are still appended for every
/// delta before the message is committed — a crash between "settled" and
/// "committed" is then still recoverable from the frames.
pub struct SettlingEmitter {
    inner: Arc<dyn AgentEmitter>,
    /// The frame recorder, kept as a concrete handle so a committed message can
    /// retire its own stream. Forwarding still goes through `inner`.
    recorder: Arc<crate::frame_progress::FrameRecordingEmitter>,
    state: Arc<SettleState>,
    harness: AgentHarness,
    signal: tokio_util::sync::CancellationToken,
    observer: Arc<dyn rpi_agent::tool_program_observation::ToolProgramObserver>,
}

impl SettlingEmitter {
    pub fn new(
        inner: Arc<dyn AgentEmitter>,
        recorder: Arc<crate::frame_progress::FrameRecordingEmitter>,
        state: Arc<SettleState>,
        harness: AgentHarness,
        signal: tokio_util::sync::CancellationToken,
    ) -> Self {
        // Pin correlation and archival writes to the same native binding. A
        // facade switch must not redirect either half to a different session.
        let session = harness
            .session()
            .pin(harness.session().id_generator().clone());
        let lane = harness.lane_name().to_owned();
        let correlate: crate::tool_program_journal::Correlate = {
            let state = state.clone();
            let session = session.clone();
            let lane = lane.clone();
            Arc::new(move |child| {
                let state = state.clone();
                let session = session.clone();
                let lane = lane.clone();
                Box::pin(async move { state.child_origin(&session, &lane, &child).await })
            })
        };
        let fail: crate::tool_program_journal::Fail = {
            let state = state.clone();
            let signal = signal.clone();
            Arc::new(move |error| {
                state.identity_error.lock().unwrap().get_or_insert(error);
                signal.cancel();
            })
        };
        let journal = Arc::new(crate::tool_program_journal::JournalObserver::new(
            session,
            lane,
            correlate,
            fail.clone(),
        ));
        let observer: Arc<dyn rpi_agent::tool_program_observation::ToolProgramObserver> =
            if let Some(extra) = inner.tool_program_observer() {
                Arc::new(crate::tool_program_observers::CombinedObserver {
                    journal,
                    extra,
                    fail,
                })
            } else {
                journal
            };
        Self {
            inner,
            recorder,
            state,
            harness,
            signal,
            observer,
        }
    }

    fn fail_identity(&self, error: impl std::fmt::Display) {
        let mut failure = self.state.identity_error.lock().unwrap();
        if failure.is_none() {
            *failure = Some(error.to_string());
        }
        self.signal.cancel();
    }

    /// Commit one settled assistant message and record its `step_attempt`.
    async fn settle_assistant(
        &self,
        index: usize,
        stream_index: usize,
        message: &AssistantMessage,
    ) {
        let entry = ProvisionedEntry {
            id: self.state.reserve_entry_id(index, &self.harness),
            kind: ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(message.clone())),
                terminate: None,
            },
        };
        let entry_id = entry.id.clone();
        if let Err(error) = self.state.prepare_commit_for_assistant(index).await {
            self.fail_identity(&error);
            tracing::warn!(%error, "could not bind a settled assistant stream");
            return;
        }
        let entry_started = self.state.diagnostics.start();
        let attempt = self.state.attempt.load(Ordering::Acquire) + 1;
        let entry_result = self
            .harness
            .append_bound_provisioned_with_step_attempt(
                entry,
                &self.state.run_id,
                attempt,
                stream_index,
            )
            .await;
        self.state
            .diagnostics
            .record(SettlePhase::AssistantEntryStep, entry_started);
        match entry_result {
            Ok(_) => {
                self.state.record_commit(index, &entry_id);
                self.state.record_tool_calls(&entry_id, message);
                // The message is an entry now, so its frames are no longer
                // progress. Retiring the stream is what stops a crash in a *later*
                // turn from replaying this one and duplicating it.
                let clear_started = self.state.diagnostics.start();
                let clear_result = self.recorder.clear_stream(stream_index).await;
                self.state
                    .diagnostics
                    .record(SettlePhase::ClearStream, clear_started);
                if let Err(error) = clear_result {
                    tracing::warn!(
                        stream_index,
                        %error,
                        "could not retire a committed assistant stream"
                    );
                }
            }
            Err(error) => {
                self.fail_identity(&error);
                tracing::warn!(%error, "could not commit a settled assistant message");
            }
        }
    }

    /// Commit any injected message under the id its queue record reserved.
    ///
    /// Delivery reservations were explicitly attached at MessageStart, before
    /// role-based settlement, and remain available for fallback persistence.
    async fn settle_injected(&self, index: usize, message: &AgentMessage) {
        let entry_id = self.state.reserve_entry_id(index, &self.harness);
        let assistant = self.state.uncommitted_assistant.lock().unwrap().take();
        if let Some((assistant_index, stream_index, assistant)) = assistant {
            let already_committed = self
                .state
                .committed
                .lock()
                .unwrap()
                .contains(&assistant_index);
            if !already_committed {
                let assistant_id = self.state.reserve_entry_id(assistant_index, &self.harness);
                if let Err(error) = self
                    .state
                    .prepare_commit(assistant_index, &assistant_id, &self.harness)
                    .await
                {
                    self.fail_identity(&error);
                    tracing::warn!(%error, "could not bind the assistant before queue injection");
                    return;
                }
                let entry = ProvisionedEntry {
                    id: assistant_id.clone(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::Assistant(Box::new(assistant)),
                        terminate: None,
                    },
                };
                if let Err(error) = self.harness.append_provisioned(entry).await {
                    self.fail_identity(&error);
                    tracing::warn!(%error, "could not commit the assistant before queue injection");
                    return;
                }
                self.state.record_commit(assistant_index, &assistant_id);
            }
            if let Err(error) = self.recorder.clear_stream(stream_index).await {
                tracing::warn!(%error, "could not retire the assistant before queue injection");
            }
        }
        let entry = ProvisionedEntry {
            id: entry_id.clone(),
            kind: ProvisionedKind::Message {
                message: message.clone(),
                terminate: None,
            },
        };
        if let Err(error) = self
            .state
            .prepare_commit(index, &entry_id, &self.harness)
            .await
        {
            self.fail_identity(&error);
            tracing::warn!(%error, "could not bind an injected assistant stream");
            return;
        }
        match self.harness.append_provisioned(entry).await {
            Ok(_) => {
                self.state.record_commit(index, &entry_id);
            }
            Err(error) => {
                self.fail_identity(&error);
                tracing::warn!(%error, "could not commit an injected message");
            }
        }
    }

    /// Commit a settled tool result under its MessageStart reservation, which
    /// reuses the ToolStarted result id when an invocation actually started.
    async fn settle_tool_result(&self, index: usize, message: &rpi_ai::types::ToolResultMessage) {
        let Some(result_entry_id) = self
            .state
            .reserved_entries
            .lock()
            .unwrap()
            .get(&index)
            .cloned()
        else {
            self.fail_identity("tool result has no MessageStart reservation");
            return;
        };
        // Preserve the tool_started identity even if this append fails. Both
        // context-boundary and run-end fallback writes resolve IDs by message
        // index, not by tool call ID.
        self.state
            .reserved_entries
            .lock()
            .unwrap()
            .insert(index, result_entry_id.clone());
        if self.state.ensure_commit_prefix(index).is_err() {
            // A previous result could not be written. Keep source order: the
            // turn-boundary fallback will persist the whole pending prefix.
            return;
        }
        let entry = ProvisionedEntry {
            id: result_entry_id.clone(),
            kind: ProvisionedKind::Message {
                message: AgentMessage::ToolResult(Box::new(message.clone())),
                terminate: None,
            },
        };
        let result_entry_started = self.state.diagnostics.start();
        let result_entry_result = self.harness.append_provisioned(entry).await;
        self.state
            .diagnostics
            .record(SettlePhase::ToolResultEntry, result_entry_started);
        match result_entry_result {
            Ok(_) => {
                self.state.record_commit(index, &result_entry_id);
            }
            Err(error) => {
                // The native reservation still exists. A refused entry ACK is
                // not lost identity: let the ordered boundary/run-end fallback
                // retry this same ID before later messages. Persistent failure
                // propagates there without completing the run or replaying the
                // tool. Missing reservations above still fail closed.
                tracing::warn!(%error, "could not commit a settled tool result");
            }
        }
    }

    fn handle(&self, event: &AgentEvent, injected_start: bool) -> Option<BoxFuture<'static, ()>> {
        // Recognize queue deliveries before dispatching on role: a stored
        // assistant/tool-result is an injection, not a new provider generation
        // or a tool invocation produced by this run.
        if let AgentEvent::MessageEnd { message, .. } = event {
            let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
            if self.state.injected_indices.lock().unwrap().contains(&index) {
                let emitter = Self {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                    signal: self.signal.clone(),
                    observer: self.observer.clone(),
                };
                let message = message.clone();
                return Some(Box::pin(async move {
                    emitter.settle_injected(index, &message).await;
                }));
            }
        }
        match event {
            AgentEvent::MessageStart {
                message,
                message_id,
            } => {
                let index = self.state.seen.fetch_add(1, Ordering::AcqRel);
                // Only the explicit queue-delivery surface transfers a queued
                // reservation. An upstream ID alone is not a delivery marker.
                if injected_start {
                    let Some(entry_id) = message_id else {
                        self.fail_identity("queue delivery has no reservation");
                        return None;
                    };
                    self.state
                        .reserved_entries
                        .lock()
                        .unwrap()
                        .insert(index, entry_id.clone());
                    self.state.injected_indices.lock().unwrap().insert(index);
                } else if let AgentMessage::ToolResult(result) = message {
                    // The native loop rejects malformed call names/IDs with
                    // End + ToolResult but deliberately no execution Start:
                    // no side effect began, so no ToolStarted record is needed.
                    // Always honor an actual Start's reservation; only a result
                    // with no Start gets a new native session entry id here.
                    // start_tool still fails closed on missing registration,
                    // and emit_reserved suppresses events after that failure.
                    let id = self
                        .state
                        .reserved_results
                        .lock()
                        .unwrap()
                        .remove(&result.tool_call_id)
                        .unwrap_or_else(|| self.harness.next_entry_id());
                    self.state
                        .reserved_entries
                        .lock()
                        .unwrap()
                        .insert(index, id);
                }
                if !injected_start && matches!(message, AgentMessage::Assistant(_)) {
                    // New native generations must not borrow a prior batch's
                    // registrations when a provider reuses its call IDs.
                    self.state.pending_calls.lock().unwrap().clear();
                    self.state.reserved_results.lock().unwrap().clear();
                    let stream_index = self.state.assistant_settled.load(Ordering::Acquire);
                    self.state
                        .assistant_streams
                        .lock()
                        .unwrap()
                        .insert(index, stream_index);
                    if let Some(id) = self.state.retry_result.lock().unwrap().take() {
                        self.state
                            .reserved_entries
                            .lock()
                            .unwrap()
                            .insert(index, id);
                    }
                }
                None
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(assistant),
                ..
            } => {
                // `seen` was advanced by this message's `MessageStart`; the index
                // of the message that just settled is one less.
                let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
                let retrying = will_retry(
                    assistant,
                    self.state.retry_enabled,
                    self.state.attempt.load(Ordering::Acquire),
                    self.state.max_retries,
                );
                let stream_index = self.state.assistant_settled.fetch_add(1, Ordering::AcqRel);
                self.state
                    .assistant_streams
                    .lock()
                    .unwrap()
                    .insert(index, stream_index);
                if !needs_early_commit(assistant, retrying) {
                    if !retrying {
                        *self.state.uncommitted_assistant.lock().unwrap() =
                            Some((index, stream_index, (**assistant).clone()));
                    }
                    return None;
                }
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                    signal: self.signal.clone(),
                    observer: self.observer.clone(),
                };
                // Ordinal first, then increment: the message that just settled is
                // stream `ordinal`.
                let message = (**assistant).clone();
                Some(Box::pin(async move {
                    emitter
                        .settle_assistant(index, stream_index, &message)
                        .await;
                }))
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(result),
                ..
            } => {
                let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                    signal: self.signal.clone(),
                    observer: self.observer.clone(),
                };
                let result = (**result).clone();
                Some(Box::pin(async move {
                    emitter.settle_tool_result(index, &result).await;
                }))
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                // Start is observable before authorization/preparation. Validate
                // its native owner here, but journal intent only in the execute
                // barrier, after effective arguments and approval are known.
                if !self
                    .state
                    .pending_calls
                    .lock()
                    .unwrap()
                    .contains_key(tool_call_id)
                {
                    self.fail_identity("tool assistant was not committed");
                }
                None
            }
            AgentEvent::AgentEnd { .. } => {
                // The final assistant message of a completed or aborted run is
                // persisted by `run_core` AFTER its `MessageEnd` broadcast, so
                // the log can still be missing the run's last turn here. Commit
                // it now, before `AgentEnd` itself is forwarded: every consumer
                // of the broadcast can then treat `AgentEnd` as the point where
                // the durable log is complete for this run (the TUI's footer
                // reconciliation is exactly such a consumer). Same fallback the
                // injected-delivery path uses, minus an injection to deliver.
                // `prepare_commit`/`append_provisioned` need await, so this runs
                // inside the future `emit` polls, not in `handle`.
                if self.state.uncommitted_assistant.lock().unwrap().is_none() {
                    return None;
                }
                let emitter = Self {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                    signal: self.signal.clone(),
                    observer: self.observer.clone(),
                };
                Some(Box::pin(async move {
                    emitter.settle_final_assistant().await;
                }))
            }
            _ => None,
        }
    }

    /// Commit the run's final assistant message before `AgentEnd` is forwarded.
    /// Called with the lock NOT held: everything here awaits.
    async fn settle_final_assistant(&self) {
        let Some((assistant_index, stream_index, assistant)) =
            self.state.uncommitted_assistant.lock().unwrap().take()
        else {
            return;
        };
        let already_committed = self
            .state
            .committed
            .lock()
            .unwrap()
            .contains(&assistant_index);
        if already_committed {
            // The normal settle path beat us to it; only the stream's frames
            // still need retiring.
            if let Err(error) = self.recorder.clear_stream(stream_index).await {
                tracing::warn!(
                    stream_index,
                    %error,
                    "could not retire the final assistant before AgentEnd"
                );
            }
            return;
        }
        let assistant_id = self.state.reserve_entry_id(assistant_index, &self.harness);
        if let Err(error) = self
            .state
            .prepare_commit(assistant_index, &assistant_id, &self.harness)
            .await
        {
            // The message stays uncommitted, so `run_core`'s persistence pass
            // will still write it after the emitter returns.
            self.fail_identity(&error);
            tracing::warn!(%error, "could not bind the final assistant before AgentEnd");
            return;
        }
        let entry = ProvisionedEntry {
            id: assistant_id.clone(),
            kind: ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(assistant)),
                terminate: None,
            },
        };
        match self.harness.append_provisioned(entry).await {
            Ok(_) => {
                self.state.record_commit(assistant_index, &assistant_id);
            }
            Err(error) => {
                // The native ID is already bound. An entry ACK refusal is not
                // identity loss: keep the reservation for `run_core`'s ordered
                // persistence pass to retry. A persistent refusal still returns
                // an error there without completing the run or clearing frames.
                // Hold AgentEnd until that whole ordered persistence pass ACKs.
                *self.state.final_assistant_barrier.lock().unwrap() =
                    FinalAssistantBarrier::AwaitingPersistence;
                tracing::warn!(
                    %error,
                    "could not commit the final assistant before AgentEnd"
                );
                return;
            }
        }
        if let Err(error) = self.recorder.clear_stream(stream_index).await {
            tracing::warn!(
                stream_index,
                %error,
                "could not retire the final assistant before AgentEnd"
            );
        }
    }
}

impl SettlingEmitter {
    fn emit_reserved(&self, mut event: AgentEvent, injected_start: bool) -> BoxFuture<'static, ()> {
        if self.state.identity_error().is_some() {
            return Box::pin(async {});
        }
        let inner = Arc::clone(&self.inner);
        let harness = self.harness.clone();
        let signal = self.signal.clone();
        let state = Arc::clone(&self.state);
        let emit_started = state.diagnostics.start();
        let handle_started = state.diagnostics.start();
        let settles = self.handle(&event, injected_start);
        let starting = matches!(&event, AgentEvent::MessageStart { .. });
        let index = state.seen.load(Ordering::Acquire).saturating_sub(1);
        let injected = matches!(
            &event,
            AgentEvent::MessageStart { .. }
                | AgentEvent::MessageUpdate { .. }
                | AgentEvent::MessageEnd { .. }
        ) && state.injected_indices.lock().unwrap().contains(&index);
        match &mut event {
            AgentEvent::MessageStart { message_id, .. }
            | AgentEvent::MessageUpdate { message_id, .. }
            | AgentEvent::MessageEnd { message_id, .. } => {
                *message_id = Some(state.reserve_entry_id(index, &harness));
            }
            _ => {}
        }
        state
            .diagnostics
            .record(SettlePhase::EmitHandle, handle_started);
        Box::pin(async move {
            if state.identity_error().is_some() {
                return;
            }
            if starting && state.stream_at(index).is_some() {
                let id = state.reserve_entry_id(index, &harness);
                if let Err(error) = state.prepare_commit(index, &id, &harness).await {
                    *state.identity_error.lock().unwrap() = Some(error.to_string());
                    signal.cancel();
                    return; // Never publish an unbound identity or substitute a new ID.
                }
            }
            // Commit *before* forwarding: a UI that renders the settled message
            // must not be able to observe it before it is durable.
            let settle_started = state.diagnostics.start();
            if let Some(settles) = settles {
                settles.await;
            }
            if state.identity_error().is_some() {
                return;
            }
            state
                .diagnostics
                .record(SettlePhase::SettleAwait, settle_started);
            if matches!(&event, AgentEvent::AgentEnd { .. }) {
                let mut barrier = state.final_assistant_barrier.lock().unwrap();
                if !matches!(*barrier, FinalAssistantBarrier::Ready) {
                    *barrier = FinalAssistantBarrier::Pending(Box::new(event));
                    return;
                }
            }
            let inner_started = state.diagnostics.start();
            if injected {
                inner.emit_injected(event).await;
            } else {
                inner.emit(event).await;
            }
            state
                .diagnostics
                .record(SettlePhase::InnerEmit, inner_started);
            state.diagnostics.record(SettlePhase::Emit, emit_started);
        })
    }
}

impl AgentEmitter for SettlingEmitter {
    fn tool_program_observer(
        &self,
    ) -> Option<Arc<dyn rpi_agent::tool_program_observation::ToolProgramObserver>> {
        Some(self.observer.clone())
    }

    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        self.emit_reserved(event, false)
    }

    fn emit_injected(&self, mut event: AgentEvent) -> BoxFuture<'static, ()> {
        if let AgentEvent::MessageStart { message_id, .. } = &mut event {
            let reservation = self.state.injected.lock().unwrap().pop_front();
            let Some(id) = reservation else {
                *self.state.identity_error.lock().unwrap() =
                    Some("queue delivery has no reservation".into());
                self.signal.cancel();
                return Box::pin(async {});
            };
            *message_id = Some(id);
        }
        self.emit_reserved(event, true)
    }

    fn try_emit(&self, event: AgentEvent) {
        if self.state.identity_error().is_none() {
            self.inner.try_emit(event);
        }
    }
}
