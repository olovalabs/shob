//! Durable assistant-message progress: record every streamed frame, and salvage
//! what committed if the run dies.
//!
//! Ports the *write* and *recovery* halves of upstream's frame progress
//! (`harness/runtime/progress.ts::openFrameProgress` +
//! `harness/runtime/drive/recovery.ts::recoverAssistantGeneration`).
//!
//! # Storage choice
//!
//! Native keeps frames in session-scoped **list** storage
//! (`pendingAssistantFrames(operationId, responseEntryId)`), separate from the
//! branch. rpi uses the independent `SessionStorage` progress APIs: Memory live
//! state, SQLite progress tables, or a reclaimable JSONL sidecar. Payloads never
//! enter transcript entries, the history sequence, or record pagination.
//!
//! Only `AssistantFrameOp::CommitStream` bindings remain in the immutable
//! history. Their reserved entry IDs and ordering make recovery idempotent even
//! if an entry commit succeeds but retiring its progress fails.
//!
//! # Model
//!
//! - Each streamed assistant message gets a `stream_index` within its run.
//! - Every frame is queued for an independent `Append` progress write.
//! - Commit and error-recovery boundaries flush the queue before consuming it.
//! - When the run's messages are persisted, one `ClearRun` retires the whole
//!   run's frames: they were progress, not history.
//! - If the run dies first, [`salvage_run_frames`] replays the committed prefix
//!   into the partial message and marks it interrupted, so the content the model
//!   had already produced is kept instead of vanishing.
//!
//! Before this, a run persisted nothing until it finished, so a crash after 14
//! minutes of work left only the user's prompt. See
//! `docs/debugging/llm-repetition-forensics.md` §十一.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use rpi_agent::{AgentEmitter, AgentEvent, AgentMessage};
use rpi_ai::frames::{reduce_frames, AssistantMessageFrame, AssistantMessageFrameEncoder};
use rpi_ai::types::{AssistantMessage, AssistantMessageEvent, StopReason, Usage};
use tokio::sync::{mpsc, oneshot};

use crate::result::{HarnessError, HarnessResult};
use crate::session::session::Session;
use crate::session::types::{
    AssistantFrameOp, AssistantFrameRecord, Entry, EntryOrder, EntryQuery, LaneRecord, RecordBase,
    RecordQuery, ToolStartedRecord,
};

const FRAME_CHUNK_RECORDS: usize = 32;
const FRAME_CHANNEL_CAPACITY: usize = 128;
const FRAME_CHECKPOINT_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy)]
pub(crate) enum FrameFlushReason {
    #[cfg(test)]
    Manual,
    PrepareCommit,
    ErrorRecovery,
    ClearStreamBefore,
    ClearStreamAfter,
    ClearRunBefore,
    ClearRunAfter,
}

#[derive(Debug, Clone, Default)]
pub struct FrameDiagnosticsSnapshot {
    pub frame_append_requests: u64,
    pub batch_count: u64,
    pub batch_records: u64,
    pub append_records_calls: u64,
    pub flush_requests: u64,
    pub queue_wait_ns: u64,
    pub flush_wait_ns: u64,
    pub batch_append_ns: u64,
    pub writer_failures: u64,
    pub prepare_commit_flushes: u64,
    pub error_recovery_flushes: u64,
    pub clear_stream_before_flushes: u64,
    pub clear_stream_after_flushes: u64,
    pub clear_run_before_flushes: u64,
    pub clear_run_after_flushes: u64,
}

#[derive(Debug, Default)]
struct FrameDiagnostics {
    enabled: bool,
    frame_append_requests: AtomicU64,
    batch_count: AtomicU64,
    batch_records: AtomicU64,
    append_records_calls: AtomicU64,
    flush_requests: AtomicU64,
    queue_wait_ns: AtomicU64,
    flush_wait_ns: AtomicU64,
    batch_append_ns: AtomicU64,
    writer_failures: AtomicU64,
    prepare_commit_flushes: AtomicU64,
    error_recovery_flushes: AtomicU64,
    clear_stream_before_flushes: AtomicU64,
    clear_stream_after_flushes: AtomicU64,
    clear_run_before_flushes: AtomicU64,
    clear_run_after_flushes: AtomicU64,
}

impl FrameDiagnostics {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            enabled: std::env::var_os("RPI_FRAME_DIAGNOSTICS").is_some_and(|value| value != "0"),
            ..Default::default()
        })
    }

    fn add_duration(counter: &AtomicU64, duration: Duration) {
        counter.fetch_add(
            duration.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }

    fn barrier(&self, reason: FrameFlushReason) {
        if !self.enabled {
            return;
        }
        let counter = match reason {
            #[cfg(test)]
            FrameFlushReason::Manual => return,
            FrameFlushReason::PrepareCommit => &self.prepare_commit_flushes,
            FrameFlushReason::ErrorRecovery => &self.error_recovery_flushes,
            FrameFlushReason::ClearStreamBefore => &self.clear_stream_before_flushes,
            FrameFlushReason::ClearStreamAfter => &self.clear_stream_after_flushes,
            FrameFlushReason::ClearRunBefore => &self.clear_run_before_flushes,
            FrameFlushReason::ClearRunAfter => &self.clear_run_after_flushes,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> FrameDiagnosticsSnapshot {
        FrameDiagnosticsSnapshot {
            frame_append_requests: self.frame_append_requests.load(Ordering::Relaxed),
            batch_count: self.batch_count.load(Ordering::Relaxed),
            batch_records: self.batch_records.load(Ordering::Relaxed),
            append_records_calls: self.append_records_calls.load(Ordering::Relaxed),
            flush_requests: self.flush_requests.load(Ordering::Relaxed),
            queue_wait_ns: self.queue_wait_ns.load(Ordering::Relaxed),
            flush_wait_ns: self.flush_wait_ns.load(Ordering::Relaxed),
            batch_append_ns: self.batch_append_ns.load(Ordering::Relaxed),
            writer_failures: self.writer_failures.load(Ordering::Relaxed),
            prepare_commit_flushes: self.prepare_commit_flushes.load(Ordering::Relaxed),
            error_recovery_flushes: self.error_recovery_flushes.load(Ordering::Relaxed),
            clear_stream_before_flushes: self.clear_stream_before_flushes.load(Ordering::Relaxed),
            clear_stream_after_flushes: self.clear_stream_after_flushes.load(Ordering::Relaxed),
            clear_run_before_flushes: self.clear_run_before_flushes.load(Ordering::Relaxed),
            clear_run_after_flushes: self.clear_run_after_flushes.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
enum FrameWriteCommand {
    Append(LaneRecord),
    Flush(oneshot::Sender<Result<(), String>>),
}

#[derive(Clone)]
struct FrameWriter {
    tx: mpsc::Sender<FrameWriteCommand>,
    diagnostics: Arc<FrameDiagnostics>,
    /// Number of frame records accepted by the channel but not yet included in
    /// a completed storage append. A barrier with no pending records is already
    /// ordered after all prior appends, so sending an empty Flush is redundant.
    pending: Arc<AtomicUsize>,
    failure: Arc<Mutex<Option<String>>>,
}

impl FrameWriter {
    fn new(session: Session) -> Self {
        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let diagnostics = FrameDiagnostics::new();
        let pending = Arc::new(AtomicUsize::new(0));
        let failure = Arc::new(Mutex::new(None));
        // A writer outlives the recorder and may finish after the harness
        // switches the shared facade to another session. Pin the original
        // storage/recovery binding, while retaining the caller's generator for
        // record ids.
        let pinned = session.pin(Arc::clone(session.id_generator()));
        tokio::spawn(run_frame_writer(
            pinned,
            rx,
            Arc::clone(&diagnostics),
            Arc::clone(&pending),
            Arc::clone(&failure),
        ));
        Self {
            tx,
            diagnostics,
            pending,
            failure,
        }
    }

    async fn append(&self, record: LaneRecord) -> HarnessResult<()> {
        let started = self.diagnostics.enabled.then(Instant::now);
        if self.diagnostics.enabled {
            self.diagnostics
                .frame_append_requests
                .fetch_add(1, Ordering::Relaxed);
        }
        let permit = self
            .tx
            .reserve()
            .await
            .map_err(|_| HarnessError::io("frame writer stopped"))?;
        // No await remains between reserve and send. Cancellation before the
        // permit is acquired never changes pending; cancellation after this
        // point cannot interrupt the synchronous increment/send pair.
        self.pending.fetch_add(1, Ordering::AcqRel);
        permit.send(FrameWriteCommand::Append(record));
        if let Some(started) = started {
            FrameDiagnostics::add_duration(&self.diagnostics.queue_wait_ns, started.elapsed());
        }
        Ok(())
    }

    async fn flush(&self) -> HarnessResult<()> {
        // `append` increments before enqueueing, and the writer decrements only
        // after its storage append completes. Therefore zero is a real ordered
        // no-op, not merely an observation that the channel happens to be empty.
        if self.pending.load(Ordering::Acquire) == 0 {
            if let Some(error) = self.failure.lock().unwrap().clone() {
                return Err(HarnessError::io(error));
            }
            if self.tx.is_closed() {
                return Err(HarnessError::io("frame writer stopped"));
            }
            return Ok(());
        }
        let started = self.diagnostics.enabled.then(Instant::now);
        if self.diagnostics.enabled {
            self.diagnostics
                .flush_requests
                .fetch_add(1, Ordering::Relaxed);
        }
        let (reply, result) = oneshot::channel();
        let send_result = self.tx.send(FrameWriteCommand::Flush(reply)).await;
        let result = match send_result {
            Ok(()) => result
                .await
                .map_err(|_| HarnessError::io("frame writer stopped"))?
                .map_err(HarnessError::io),
            Err(_) => Err(HarnessError::io("frame writer stopped")),
        };
        if let Some(started) = started {
            FrameDiagnostics::add_duration(&self.diagnostics.flush_wait_ns, started.elapsed());
        }
        result
    }
}

async fn run_frame_writer(
    session: Session,
    mut rx: mpsc::Receiver<FrameWriteCommand>,
    diagnostics: Arc<FrameDiagnostics>,
    pending: Arc<AtomicUsize>,
    failure_state: Arc<Mutex<Option<String>>>,
) {
    let mut batch = Vec::with_capacity(FRAME_CHUNK_RECORDS);
    let mut deadline = None;
    let mut failure: Option<String> = None;

    loop {
        let command = if batch.is_empty() {
            deadline = None;
            rx.recv().await
        } else {
            let wait_until = deadline.expect("a non-empty frame batch has a deadline");
            match tokio::time::timeout_at(wait_until, rx.recv()).await {
                Ok(command) => command,
                Err(_) => {
                    if failure.is_none() {
                        if let Err(error) = flush_frame_batch(
                            &session,
                            &mut batch,
                            &diagnostics,
                            &pending,
                            &failure_state,
                        )
                        .await
                        {
                            diagnostics.writer_failures.fetch_add(1, Ordering::Relaxed);
                            let message = error.to_string();
                            *failure_state.lock().unwrap() = Some(message.clone());
                            failure = Some(message);
                        }
                    }
                    deadline = None;
                    continue;
                }
            }
        };

        let Some(command) = command else {
            break;
        };
        match command {
            FrameWriteCommand::Append(record) => {
                if failure.is_none() {
                    // The checkpoint is measured from the first pending frame,
                    // not from the most recent receive. A busy stream must not
                    // postpone durability indefinitely.
                    if batch.is_empty() {
                        deadline = Some(tokio::time::Instant::now() + FRAME_CHECKPOINT_INTERVAL);
                    }
                    batch.push(record);
                    if batch.len() >= FRAME_CHUNK_RECORDS {
                        if let Err(error) = flush_frame_batch(
                            &session,
                            &mut batch,
                            &diagnostics,
                            &pending,
                            &failure_state,
                        )
                        .await
                        {
                            diagnostics.writer_failures.fetch_add(1, Ordering::Relaxed);
                            let message = error.to_string();
                            *failure_state.lock().unwrap() = Some(message.clone());
                            failure = Some(message);
                        }
                        deadline = None;
                    }
                } else {
                    // The writer has already reported a durable failure. Accept
                    // no more work, but release its pending accounting so a
                    // caller never waits on an impossible batch.
                    pending.fetch_sub(1, Ordering::AcqRel);
                }
            }
            FrameWriteCommand::Flush(reply) => {
                if failure.is_none() {
                    if let Err(error) = flush_frame_batch(
                        &session,
                        &mut batch,
                        &diagnostics,
                        &pending,
                        &failure_state,
                    )
                    .await
                    {
                        diagnostics.writer_failures.fetch_add(1, Ordering::Relaxed);
                        let message = error.to_string();
                        *failure_state.lock().unwrap() = Some(message.clone());
                        failure = Some(message);
                    }
                }
                deadline = None;
                let result = failure.clone().map_or(Ok(()), Err);
                let _ = reply.send(result);
            }
        }
    }

    if failure.is_none() {
        let _ =
            flush_frame_batch(&session, &mut batch, &diagnostics, &pending, &failure_state).await;
    }
}

fn publish_failure_before_pending_release(
    message: String,
    failure_state: &Mutex<Option<String>>,
    pending: &AtomicUsize,
    count: usize,
) {
    let mut failure = failure_state.lock().unwrap();
    if failure.is_none() {
        *failure = Some(message);
    }
    drop(failure);
    pending.fetch_sub(count, Ordering::AcqRel);
}

async fn flush_frame_batch(
    session: &Session,
    batch: &mut Vec<LaneRecord>,
    diagnostics: &FrameDiagnostics,
    pending: &AtomicUsize,
    failure_state: &Mutex<Option<String>>,
) -> HarnessResult<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let records = std::mem::take(batch);
    let count = records.len();
    if diagnostics.enabled {
        diagnostics.batch_count.fetch_add(1, Ordering::Relaxed);
        diagnostics
            .batch_records
            .fetch_add(count as u64, Ordering::Relaxed);
        diagnostics
            .append_records_calls
            .fetch_add(1, Ordering::Relaxed);
    }
    let started = diagnostics.enabled.then(Instant::now);
    let frames = records
        .into_iter()
        .map(|record| match record {
            LaneRecord::AssistantFrame(frame) => frame,
            _ => unreachable!("frame writer only accepts assistant progress"),
        })
        .collect();
    let result = session
        .append_progress(frames)
        .await
        .map(|_| ())
        .map_err(|error| {
            HarnessError::io(format!("could not append assistant frame chunk: {error}"))
        });
    if let Err(error) = &result {
        // Publish failure before releasing the last pending count. A concurrent
        // barrier must not observe zero and return success in this small window.
        publish_failure_before_pending_release(error.to_string(), failure_state, pending, count);
    } else {
        pending.fetch_sub(count, Ordering::AcqRel);
    }
    if let Some(started) = started {
        FrameDiagnostics::add_duration(&diagnostics.batch_append_ns, started.elapsed());
    }
    result
}

/// The `record_type` tag for [`AssistantFrameRecord`].
pub const ASSISTANT_FRAME_RECORD_TYPE: &str = "assistant_frame";

/// The text upstream attaches to a message reconstructed from frames. Kept
/// verbatim so a reader (or a tool) can recognise the case.
pub const INTERRUPTED_NOTICE: &str = "Assistant request was interrupted. The preceding content is \
the latest committed partial; newer live output may be missing and the external outcome is unknown.";

#[derive(Clone, Default)]
pub(crate) struct VolatileStream {
    frames: Vec<AssistantMessageFrame>,
    committed_entries: Vec<String>,
}
#[derive(Default)]
pub(crate) struct VolatileProgress {
    runs: BTreeMap<String, BTreeMap<usize, VolatileStream>>,
}
pub(crate) type SharedVolatileProgress = Arc<Mutex<VolatileProgress>>;

/// Turn a replayed partial into the interrupted message rpi records.
///
/// Mirrors `interruptedAssistantMessage`: `stopReason: error` plus the notice,
/// and **usage zeroed** so a salvaged partial cannot double-count tokens that a
/// later retry bills again.
pub fn interrupted_message(partial: AssistantMessage) -> AssistantMessage {
    AssistantMessage {
        stop_reason: StopReason::Error,
        error_message: Some(INTERRUPTED_NOTICE.to_string()),
        usage: Usage::zero(),
        ..partial
    }
}

/// Text attached to the synthetic tool result that stands in for a tool call
/// whose real result never landed. Mirrors the intent of upstream's
/// interrupted-operation notices: say plainly that the outcome is unknown, so
/// neither the user nor the model assumes the tool had no effect.
pub const INTERRUPTED_TOOL_RESULT: &str = "Tool execution was interrupted before its result was \
recorded. The external outcome is unknown: this tool may or may not have taken effect.";

/// A tool call whose result never landed, from the committed frames of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownToolOutcome {
    pub tool_call_id: String,
    pub tool_name: String,
    /// The arguments the model asked for, recovered from the frames. This is what
    /// lets a reader judge what the tool *would* have done.
    pub arguments: serde_json::Value,
    /// The call's recording when it already ran upstream. Such a call is answered
    /// from this recording and is never replayed.
    pub recorded: Option<serde_json::Value>,
}

/// The unresolved calls of one assistant on the current lane branch. A missing
/// entry id denotes an uncommitted partial, which cannot already have results.
/// Storage errors and conflicting reserved-result identities fail recovery
/// rather than silently discarding its report.
pub async fn unknown_tool_outcomes(
    session: &Session,
    lane: &str,
    assistant_entry_id: Option<&str>,
    partial: &AssistantMessage,
) -> HarnessResult<Vec<UnknownToolOutcome>> {
    let started = if assistant_entry_id.is_some()
        && partial
            .content
            .iter()
            .any(|block| matches!(block, rpi_ai::types::Content::ToolCall(_)))
    {
        session
            .find_records(&RecordQuery {
                lane: Some(lane.to_string()),
                record_type: Some("tool_started"),
                ..Default::default()
            })
            .await
            .map_err(|error| HarnessError::io(error.to_string()))?
            .into_iter()
            .filter_map(|record| match record {
                LaneRecord::ToolStarted(record) => Some(record),
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    unknown_tool_outcomes_with_started(session, lane, assistant_entry_id, partial, &started).await
}

/// Recovery already read these records for the run. Reuse them rather than
/// querying/cloning the history again for each assistant batch.
pub(crate) async fn unknown_tool_outcomes_with_started(
    session: &Session,
    lane: &str,
    assistant_entry_id: Option<&str>,
    partial: &AssistantMessage,
    started: &[ToolStartedRecord],
) -> HarnessResult<Vec<UnknownToolOutcome>> {
    let calls: Vec<&rpi_ai::types::ToolCall> = partial
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::types::Content::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        return Ok(Vec::new());
    }

    let entries = session
        .view(lane)
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                exclude_tool_program_bodies: true,
                ..Default::default()
            },
            &Default::default(),
        )
        .await
        .map_err(|error| HarnessError::io(error.to_string()))?;
    // Results belong to the assistant batch, never to a different lane or an
    // earlier assistant that happened to reuse a provider call id.
    let start = assistant_entry_id.and_then(|id| entries.iter().position(|entry| entry.id() == id));
    if assistant_entry_id.is_some() && start.is_none() {
        return Err(HarnessError::io(
            "Recovery assistant is not on the current lane branch",
        ));
    }
    let expected: BTreeMap<_, _> = started
        .iter()
        .filter(|record| Some(record.assistant_entry_id.as_str()) == assistant_entry_id)
        .map(|record| {
            (
                (record.tool_call_id.as_str(), record.tool_name.as_str()),
                record.result_entry_id.as_str(),
            )
        })
        .collect();
    let mut landed = std::collections::BTreeSet::new();
    for entry in entries
        .iter()
        .skip(start.map_or(entries.len(), |i| i + 1))
        .take_while(|entry| match entry {
            Entry::Custom(_) => true,
            Entry::Message(message) => matches!(message.message, AgentMessage::ToolResult(_)),
            _ => false,
        })
    {
        let Entry::Message(message) = entry else {
            continue;
        };
        let AgentMessage::ToolResult(result) = &message.message else {
            continue;
        };
        let key = (result.tool_call_id.as_str(), result.tool_name.as_str());
        if let Some(expected_id) = expected.get(&key) {
            if message.base.id != *expected_id {
                // Never silently adopt an external result or replay over it.
                // Validate the entire batch before ANY call can be replayed.
                return Err(HarnessError::io(format!(
                    "Recovery result identity conflict for {}:{}: expected entry {}, found {}",
                    key.0, key.1, expected_id, message.base.id
                )));
            }
        }
        landed.insert(key);
    }

    Ok(calls
        .into_iter()
        .filter(|call| !landed.contains(&(call.id.as_str(), call.name.as_str())))
        .map(|call| UnknownToolOutcome {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            recorded: call.recorded.clone(),
        })
        .collect())
}

/// The synthetic error result that stands in for an interrupted tool call.
pub fn interrupted_tool_result(outcome: &UnknownToolOutcome, timestamp: i64) -> AgentMessage {
    AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
        role: rpi_ai::types::ToolResultRole,
        tool_call_id: outcome.tool_call_id.clone(),
        tool_name: outcome.tool_name.clone(),
        content: vec![rpi_ai::types::Content::text(INTERRUPTED_TOOL_RESULT)],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error: true,
        timestamp,
    }))
}

/// The tool result for an unresolved call that already ran upstream: its
/// recording, or an error result when the recording is unusable. The call is
/// never executed here.
pub fn recorded_tool_result(
    outcome: &UnknownToolOutcome,
    recording: &serde_json::Value,
    timestamp: i64,
) -> AgentMessage {
    let (result, is_error) = match rpi_agent::agent_loop::recorded_tool_result(recording) {
        Ok(decoded) => decoded,
        Err(reason) => (rpi_agent::AgentToolResult::error_text(reason), true),
    };
    let details = Some(result.details.clone());
    AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
        role: rpi_ai::types::ToolResultRole,
        tool_call_id: outcome.tool_call_id.clone(),
        tool_name: outcome.tool_name.clone(),
        content: result.into_content(),
        details,
        usage: None,
        added_tool_names: Vec::new(),
        is_error,
        timestamp,
    }))
}

/// Reduce the committed frames of every stream in `run_id`, in stream order.
///
/// A recoverable stream retains its durable identity until its message commits.
pub(crate) struct ReplayedStream {
    pub stream_index: usize,
    pub entry_id: Option<String>,
    pub message: AssistantMessage,
}

pub(crate) async fn replay_run_frames(
    session: &Session,
    run_id: &str,
) -> HarnessResult<Vec<AssistantMessage>> {
    Ok(replay_run_streams(session, run_id)
        .await?
        .into_iter()
        .map(|s| s.message)
        .collect())
}

/// Return uncommitted streams. Cleared runs yield nothing; unreadable frame
/// sequences are skipped rather than making the entire session unreadable.
pub(crate) async fn replay_run_streams(
    session: &Session,
    run_id: &str,
) -> HarnessResult<Vec<ReplayedStream>> {
    if let Some(progress) = session.volatile_progress() {
        // Copy only on recovery. The streaming path moves typed frames into the
        // cache, retaining the same encoder/reducer semantics as durable mode.
        let streams = progress
            .lock()
            .unwrap()
            .runs
            .get(run_id)
            .cloned()
            .unwrap_or_default();
        let mut replayed = Vec::new();
        for (stream_index, stream) in streams {
            let mut committed = false;
            for entry_id in &stream.committed_entries {
                if session
                    .get_entry(entry_id)
                    .await
                    .map_err(|error| HarnessError::io(error.to_string()))?
                    .is_some()
                {
                    committed = true;
                    break;
                }
            }
            if !committed {
                match reduce_frames(&stream.frames) {
                    Ok(Some(partial)) => replayed.push(ReplayedStream {
                        stream_index,
                        entry_id: stream.committed_entries.last().cloned(),
                        message: partial,
                    }),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "discarding an unreadable in-memory assistant frame stream")
                    }
                }
            }
        }
        return Ok(replayed);
    }
    let mut records = session
        .find_records(&RecordQuery {
            record_type: Some(ASSISTANT_FRAME_RECORD_TYPE),
            run_id: Some(run_id.to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .map_err(|error| HarnessError::io(error.to_string()))?;

    records.extend(
        session
            .read_progress(run_id)
            .await
            .map_err(|error| HarnessError::io(error.to_string()))?
            .into_iter()
            .map(LaneRecord::AssistantFrame),
    );
    let mut streams: BTreeMap<usize, Vec<AssistantMessageFrame>> = BTreeMap::new();
    // Streams whose message is already an entry (committed mid-run by
    // commit-on-settle). Replaying one would duplicate it in the transcript.
    let mut cleared: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut commits: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for record in records {
        let LaneRecord::AssistantFrame(frame) = record else {
            continue;
        };
        match frame.op {
            // The run committed after this point, so its frames were only
            // progress and nothing needs salvaging.
            AssistantFrameOp::ClearRun => return Ok(Vec::new()),
            AssistantFrameOp::ClearStream => {
                cleared.insert(frame.stream_index);
            }
            AssistantFrameOp::CommitStream => {
                if let Some(entry_id) = frame
                    .frame
                    .as_ref()
                    .and_then(|value| value.get("entryId"))
                    .and_then(|value| value.as_str())
                {
                    commits
                        .entry(frame.stream_index)
                        .or_default()
                        .push(entry_id.to_string());
                }
            }
            AssistantFrameOp::Append => {
                let Some(value) = frame.frame else {
                    continue;
                };
                match serde_json::from_value::<AssistantMessageFrame>(value) {
                    Ok(decoded) => streams.entry(frame.stream_index).or_default().push(decoded),
                    Err(error) => tracing::warn!(
                        stream_index = frame.stream_index,
                        %error,
                        "skipping an undecodable assistant frame during recovery"
                    ),
                }
            }
        }
    }

    let mut replayed = Vec::new();
    for (index, frames) in streams {
        if cleared.contains(&index) {
            continue;
        }
        let mut committed = false;
        if let Some(entry_ids) = commits.get(&index) {
            for entry_id in entry_ids {
                if session
                    .get_entry(entry_id)
                    .await
                    .map_err(|error| HarnessError::io(error.to_string()))?
                    .is_some()
                {
                    committed = true;
                    break;
                }
            }
        }
        if committed {
            continue;
        }
        match reduce_frames(&frames) {
            Ok(Some(partial)) => replayed.push(ReplayedStream {
                stream_index: index,
                entry_id: commits.get(&index).and_then(|ids| ids.last()).cloned(),
                message: partial,
            }),
            Ok(None) => {}
            Err(error) => tracing::warn!(
                %error,
                "discarding an unreadable assistant frame stream during recovery"
            ),
        }
    }
    Ok(replayed)
}

/// Retire only after all recovered transcript entries have committed.
pub(crate) async fn retire_run_frames(
    session: &Session,
    lane: &str,
    run_id: &str,
) -> HarnessResult<()> {
    if let Some(progress) = session.volatile_progress() {
        progress.lock().unwrap().runs.remove(run_id);
        return Ok(());
    }
    session
        .append_progress(vec![AssistantFrameRecord {
            base: RecordBase {
                id: session.id_generator().next(),
                seq: 0,
                lane: lane.into(),
                timestamp: 0,
            },
            run_id: run_id.into(),
            stream_index: 0,
            op: AssistantFrameOp::ClearRun,
            frame: None,
        }])
        .await
        .map_err(|error| HarnessError::io(error.to_string()))?;
    Ok(())
}

/// Record the entry a stream will commit under before appending that entry.
/// Unlike a clear marker, this preserves progress if the commit never lands.
pub(crate) async fn bind_stream_entry(
    session: &Session,
    lane: &str,
    run_id: &str,
    stream_index: usize,
    entry_id: &str,
) -> HarnessResult<()> {
    if let Some(progress) = session.volatile_progress() {
        progress
            .lock()
            .unwrap()
            .runs
            .entry(run_id.to_owned())
            .or_default()
            .entry(stream_index)
            .or_default()
            .committed_entries
            .push(entry_id.to_owned());
        return Ok(());
    }
    session
        .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: session.id_generator().next(),
                seq: 0,
                lane: lane.to_string(),
                timestamp: 0,
            },
            run_id: run_id.to_string(),
            stream_index,
            op: AssistantFrameOp::CommitStream,
            frame: Some(serde_json::json!({"entryId": entry_id})),
        }))
        .await
        .map_err(|error| HarnessError::io(error.to_string()))?;
    Ok(())
}

/// Replay the committed frames of every stream in `run_id`.
///
/// Returns the salvaged messages in stream order. A run that was cleared (i.e.
/// it committed) yields nothing.
pub async fn salvage_run_frames(
    session: &Session,
    run_id: &str,
) -> HarnessResult<Vec<AssistantMessage>> {
    Ok(replay_run_frames(session, run_id)
        .await?
        .into_iter()
        .map(interrupted_message)
        .collect())
}

/// Emitter wrapper that forwards every event and durably records assistant
/// frames as they stream.
///
/// The recording state is shared behind an `Arc` so `emit` can move its clones
/// into the returned future.
pub struct FrameRecordingEmitter {
    inner: Arc<dyn AgentEmitter>,
    shared: Arc<RecorderShared>,
}

struct RecorderShared {
    session: Session,
    /// The lane the frames belong to. Required: the store validates the lane and
    /// rejects a record with an unknown one.
    lane: String,
    run_id: String,
    state: Mutex<RecorderState>,
    volatile: Option<SharedVolatileProgress>,
    writer: Option<FrameWriter>,
    diagnostics: Option<Arc<FrameDiagnostics>>,
}

struct RecorderState {
    next_index: usize,
    /// The stream currently being recorded: its index and encoder.
    current: Option<(usize, AssistantMessageFrameEncoder)>,
}

impl RecorderShared {
    /// Queue one `AssistantFrame` record for the chunk writer.
    async fn append_frame_record(
        &self,
        stream_index: usize,
        op: AssistantFrameOp,
        frame: Option<serde_json::Value>,
    ) -> HarnessResult<()> {
        let record = LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            run_id: self.run_id.clone(),
            stream_index,
            op,
            frame,
        });
        self.writer
            .as_ref()
            .ok_or_else(|| HarnessError::io("durable frame writer is unavailable"))?
            .append(record)
            .await
    }

    async fn flush_frames(&self, reason: FrameFlushReason) -> HarnessResult<()> {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.barrier(reason);
        }
        if let Some(writer) = &self.writer {
            writer.flush().await
        } else {
            Ok(())
        }
    }

    fn diagnostics_snapshot(&self) -> Option<FrameDiagnosticsSnapshot> {
        self.diagnostics
            .as_ref()
            .filter(|diagnostics| diagnostics.enabled)
            .map(|diagnostics| diagnostics.snapshot())
    }

    fn log_diagnostics(&self) {
        let Some(snapshot) = self.diagnostics_snapshot() else {
            return;
        };
        eprintln!(
            "RPI_FRAME_DIAGNOSTICS lane={} run_id={} frame_append_requests={} batch_count={} batch_records={} append_records_calls={} flush_requests={} queue_wait_ns={} flush_wait_ns={} batch_append_ns={} writer_failures={} prepare_commit_flushes={} error_recovery_flushes={} clear_stream_before_flushes={} clear_stream_after_flushes={} clear_run_before_flushes={} clear_run_after_flushes={}",
            self.lane,
            self.run_id,
            snapshot.frame_append_requests,
            snapshot.batch_count,
            snapshot.batch_records,
            snapshot.append_records_calls,
            snapshot.flush_requests,
            snapshot.queue_wait_ns,
            snapshot.flush_wait_ns,
            snapshot.batch_append_ns,
            snapshot.writer_failures,
            snapshot.prepare_commit_flushes,
            snapshot.error_recovery_flushes,
            snapshot.clear_stream_before_flushes,
            snapshot.clear_stream_after_flushes,
            snapshot.clear_run_before_flushes,
            snapshot.clear_run_after_flushes,
        );
    }

    /// Advance the recorder for one event and return the frame to append, if
    /// any. Pure bookkeeping — no I/O.
    fn take_frame(&self, event: &AssistantMessageEvent) -> Option<(usize, AssistantMessageFrame)> {
        let mut state = self.state.lock().unwrap();
        let terminal = matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        );
        if matches!(event, AssistantMessageEvent::Start { .. }) {
            if let Some((previous, _)) = state.current.take() {
                tracing::warn!(
                    stream_index = previous,
                    "assistant frame stream restarted without a terminal event"
                );
            }
            let index = state.next_index;
            state.next_index += 1;
            state.current = Some((index, AssistantMessageFrameEncoder::new()));
        }
        let (index, encoder) = state.current.as_mut()?;
        let index = *index;
        let frame = match encoder.encode(event) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(
                    stream_index = index,
                    %error,
                    "dropping a frame that violates the stream contract"
                );
                None
            }
        };
        if terminal {
            state.current = None;
        }
        frame.map(|frame| (index, frame))
    }
}

impl FrameRecordingEmitter {
    pub fn new(
        inner: Arc<dyn AgentEmitter>,
        session: Session,
        lane: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Self {
        let volatile = session.volatile_progress();
        let writer = volatile
            .is_none()
            .then(|| FrameWriter::new(session.clone()));
        let diagnostics = writer
            .as_ref()
            .map(|writer| Arc::clone(&writer.diagnostics));
        Self {
            inner,
            shared: Arc::new(RecorderShared {
                session,
                lane: lane.into(),
                run_id: run_id.into(),
                volatile,
                writer,
                diagnostics,
                state: Mutex::new(RecorderState {
                    next_index: 0,
                    current: None,
                }),
            }),
        }
    }

    /// The run this recorder is following.
    pub fn run_id(&self) -> &str {
        &self.shared.run_id
    }

    /// Flush all queued durable frame chunks before a commit boundary.
    #[cfg(test)]
    pub(crate) async fn flush(&self) -> HarnessResult<()> {
        self.shared.flush_frames(FrameFlushReason::Manual).await
    }

    pub(crate) async fn flush_with_reason(&self, reason: FrameFlushReason) -> HarnessResult<()> {
        self.shared.flush_frames(reason).await
    }

    /// Snapshot optional frame writer diagnostics.
    pub fn diagnostics(&self) -> Option<FrameDiagnosticsSnapshot> {
        self.shared.diagnostics_snapshot()
    }

    /// Retire this run's frames — called once its messages are persisted.
    pub async fn clear(&self) -> HarnessResult<()> {
        if let Some(progress) = &self.shared.volatile {
            progress.lock().unwrap().runs.remove(&self.shared.run_id);
            return Ok(());
        }
        self.shared
            .flush_frames(FrameFlushReason::ClearRunBefore)
            .await?;
        self.shared
            .append_frame_record(0, AssistantFrameOp::ClearRun, None)
            .await?;
        let result = self
            .shared
            .flush_frames(FrameFlushReason::ClearRunAfter)
            .await;
        if result.is_ok() {
            self.shared.log_diagnostics();
        }
        result
    }

    /// Retire one stream, whose message has just been committed as an entry.
    ///
    /// Without this, salvage would replay it again on a crash and duplicate the
    /// message: the run-wide `ClearRun` marker only arrives at the end of the run.
    pub async fn clear_stream(&self, stream_index: usize) -> HarnessResult<()> {
        if let Some(progress) = &self.shared.volatile {
            if let Some(run) = progress.lock().unwrap().runs.get_mut(&self.shared.run_id) {
                run.remove(&stream_index);
            }
            return Ok(());
        }
        self.shared
            .flush_frames(FrameFlushReason::ClearStreamBefore)
            .await?;
        self.shared
            .append_frame_record(stream_index, AssistantFrameOp::ClearStream, None)
            .await?;
        self.shared
            .flush_frames(FrameFlushReason::ClearStreamAfter)
            .await
    }

    /// Record one event's frame durably. Failures are logged, never fatal: a
    /// session that cannot record progress must still run the agent.
    async fn record(shared: &RecorderShared, event: &AssistantMessageEvent) {
        let Some((index, frame)) = shared.take_frame(event) else {
            return;
        };
        if let Some(progress) = &shared.volatile {
            progress
                .lock()
                .unwrap()
                .runs
                .entry(shared.run_id.clone())
                .or_default()
                .entry(index)
                .or_default()
                .frames
                .push(frame);
            return;
        }
        let value = match serde_json::to_value(&frame) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "could not serialize an assistant frame");
                return;
            }
        };
        if let Err(error) = shared
            .append_frame_record(index, AssistantFrameOp::Append, Some(value))
            .await
        {
            tracing::warn!(stream_index = index, %error, "could not append an assistant frame");
        }
    }
}

impl AgentEmitter for FrameRecordingEmitter {
    fn tool_program_observer(
        &self,
    ) -> Option<Arc<dyn rpi_agent::tool_program_observation::ToolProgramObserver>> {
        self.inner.tool_program_observer()
    }

    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let inner = Arc::clone(&self.inner);
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            // Borrow from the owned event while recording, then forward it.
            // This avoids cloning every delta string into a temporary event.
            match &event {
                AgentEvent::MessageStart {
                    message: AgentMessage::Assistant(assistant),
                    ..
                } => {
                    let start = AssistantMessageEvent::Start {
                        partial: Arc::new((**assistant).clone()),
                    };
                    FrameRecordingEmitter::record(&shared, &start).await;
                }
                AgentEvent::MessageUpdate {
                    assistant_message_event,
                    ..
                } => {
                    FrameRecordingEmitter::record(&shared, assistant_message_event).await;
                }
                AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(_),
                    ..
                } => {
                    shared.state.lock().unwrap().current = None;
                }
                _ => {}
            }
            inner.emit(event).await;
        })
    }

    fn emit_injected(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        // Queue deliveries are already complete messages, not provider streams.
        // SettlingEmitter preserves this explicit marker after assigning IDs;
        // public message_id metadata must never decide whether frames record.
        self.inner.emit_injected(event)
    }

    fn try_emit(&self, event: AgentEvent) {
        self.inner.try_emit(event);
    }
}

#[cfg(test)]
mod volatile_tests {
    use super::*;
    use crate::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
    use crate::session::types::{ProvisionedEntry, ProvisionedKind, SessionMetadata};
    use rpi_agent::CollectorEmitter;
    use rpi_ai::{Api, Content};

    fn session(volatile: bool) -> Session {
        let session = Session::new(
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
        );
        if volatile {
            session.with_volatile_progress()
        } else {
            session
        }
    }
    fn events() -> Vec<AgentEvent> {
        let mut message = AssistantMessage::empty(Api::Faux, "faux", "m", 42);
        let mut events = vec![AgentEvent::MessageStart {
            message_id: None,
            message: message.clone().into(),
        }];
        // Start snapshots may already cover a delta not yet observed by the
        // encoder. Both modes must retain the encoder's catch-up semantics.
        message.content.push(Content::text("ahead"));
        let partial = Arc::new(message.clone());
        events.push(AgentEvent::MessageUpdate {
            message_id: None,
            message: Arc::clone(&partial),
            assistant_message_event: AssistantMessageEvent::TextStart {
                content_index: 0,
                partial,
            },
        });
        for delta in ["ahead", " 🐈", "", "!"] {
            if delta != "ahead" {
                if let Content::Text(text) = &mut message.content[0] {
                    text.text.push_str(delta);
                }
            }
            let partial = Arc::new(message.clone());
            events.push(AgentEvent::MessageUpdate {
                message_id: None,
                message: Arc::clone(&partial),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.into(),
                    partial,
                },
            });
        }
        events
    }
    #[tokio::test]
    async fn volatile_recovery_matches_durable_prefixes_without_json_records() {
        let events = events();
        for prefix in 0..=events.len() {
            let durable = session(false);
            let volatile = session(true);
            for session in [&durable, &volatile] {
                let recorder = FrameRecordingEmitter::new(
                    Arc::new(CollectorEmitter::default()),
                    session.clone(),
                    "main",
                    "run",
                );
                for event in &events[..prefix] {
                    recorder.emit(event.clone()).await;
                }
                recorder.flush().await.unwrap();
            }
            assert_eq!(
                salvage_run_frames(&volatile, "run").await.unwrap(),
                salvage_run_frames(&durable, "run").await.unwrap(),
                "prefix {prefix}"
            );
            assert!(volatile
                .find_records(&RecordQuery {
                    record_type: Some(ASSISTANT_FRAME_RECORD_TYPE),
                    ..Default::default()
                })
                .await
                .unwrap()
                .is_empty());
        }
    }
    #[tokio::test]
    async fn reserved_identity_survives_binding_before_first_frame_in_both_modes() {
        for volatile in [false, true] {
            let session = session(volatile);
            bind_stream_entry(&session, "main", "run", 0, "live-id")
                .await
                .unwrap();
            let recorder = FrameRecordingEmitter::new(
                Arc::new(CollectorEmitter::default()),
                session.clone(),
                "main",
                "run",
            );
            for mut event in events() {
                match &mut event {
                    AgentEvent::MessageStart { message_id, .. }
                    | AgentEvent::MessageUpdate { message_id, .. }
                    | AgentEvent::MessageEnd { message_id, .. } => {
                        *message_id = Some("live-id".into());
                    }
                    _ => {}
                }
                recorder.emit(event).await;
            }
            recorder.flush().await.unwrap();
            let recovered = replay_run_streams(&session, "run").await.unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].entry_id.as_deref(), Some("live-id"));
            assert!(!recovered[0].message.content.is_empty());
        }
    }

    #[tokio::test]
    async fn volatile_bindings_clear_and_storage_switch_preserve_commit_boundaries() {
        for volatile in [false, true] {
            let session = session(volatile);
            let recorder = FrameRecordingEmitter::new(
                Arc::new(CollectorEmitter::default()),
                session.clone(),
                "main",
                "run",
            );
            for event in events() {
                recorder.emit(event).await;
            }
            recorder.flush().await.unwrap();
            assert_eq!(replay_run_frames(&session, "run").await.unwrap().len(), 1);
            bind_stream_entry(&session, "main", "run", 0, "settled")
                .await
                .unwrap();
            assert_eq!(
                replay_run_frames(&session, "run").await.unwrap().len(),
                1,
                "a binding alone must not hide progress"
            );
            session
                .storage()
                .append_entry(
                    ProvisionedEntry {
                        id: "settled".into(),
                        kind: ProvisionedKind::Message {
                            message: rpi_ai::UserMessage::new("committed", 42).into(),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            assert!(replay_run_frames(&session, "run").await.unwrap().is_empty());
            recorder.clear_stream(0).await.unwrap();
            recorder.clear().await.unwrap();
            assert!(replay_run_frames(&session, "run").await.unwrap().is_empty());
            if volatile {
                assert!(session
                    .volatile_progress()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .runs
                    .is_empty());
                let other = self::session(false);
                session.set_storage(other.storage());
                assert!(session.volatile_progress().is_none());
            }
        }
    }

    #[test]
    fn sticky_failure_is_published_before_pending_reaches_zero() {
        let pending = AtomicUsize::new(1);
        let failure = Mutex::new(None);
        publish_failure_before_pending_release("append failed".into(), &failure, &pending, 1);
        assert_eq!(pending.load(Ordering::Acquire), 0);
        assert_eq!(
            failure.lock().unwrap().as_deref(),
            Some("append failed"),
            "pending zero must imply the sticky failure is already visible"
        );

        let (tx, _rx) = mpsc::channel(1);
        let writer = FrameWriter {
            tx,
            diagnostics: FrameDiagnostics::new(),
            pending: Arc::new(AtomicUsize::new(0)),
            failure: Arc::new(Mutex::new(Some("append failed".into()))),
        };
        let result = futures::executor::block_on(writer.flush());
        assert!(
            result.is_err(),
            "sticky failure must not be hidden by fast path"
        );
    }

    #[tokio::test]
    async fn failed_frame_append_publishes_sticky_error_before_release() {
        let session = session(false);
        let pending = AtomicUsize::new(1);
        let failure = Mutex::new(None);
        let mut batch = vec![LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: "bad-lane-frame".into(),
                seq: 0,
                lane: "missing-lane".into(),
                timestamp: 0,
            },
            run_id: "failed-run".into(),
            stream_index: 0,
            op: AssistantFrameOp::Append,
            frame: None,
        })];

        let result = flush_frame_batch(
            &session,
            &mut batch,
            &FrameDiagnostics::new(),
            &pending,
            &failure,
        )
        .await;
        assert!(result.is_err());
        assert!(batch.is_empty());
        assert_eq!(pending.load(Ordering::Acquire), 0);
        assert!(failure.lock().unwrap().is_some());
    }
    #[tokio::test]
    async fn cancelled_append_does_not_leak_pending_reservation() {
        let (tx, mut rx) = mpsc::channel(1);
        let pending = Arc::new(AtomicUsize::new(0));
        let writer = FrameWriter {
            tx,
            diagnostics: FrameDiagnostics::new(),
            pending: Arc::clone(&pending),
            failure: Arc::new(Mutex::new(None)),
        };
        let record = |id: String| {
            LaneRecord::AssistantFrame(AssistantFrameRecord {
                base: RecordBase {
                    id,
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                run_id: "cancel-run".into(),
                stream_index: 0,
                op: AssistantFrameOp::Append,
                frame: None,
            })
        };

        writer.append(record("first".into())).await.unwrap();
        assert_eq!(pending.load(Ordering::Acquire), 1);
        let blocked_writer = writer.clone();
        let mut blocked = Box::pin(blocked_writer.append(record("cancelled".into())));
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        drop(blocked);
        assert_eq!(
            pending.load(Ordering::Acquire),
            1,
            "a cancelled reserve must not leave a phantom pending record"
        );

        let command = rx.recv().await.expect("the first append is queued");
        assert!(matches!(command, FrameWriteCommand::Append(_)));
        pending.fetch_sub(1, Ordering::AcqRel);
        assert_eq!(pending.load(Ordering::Acquire), 0);
        writer.flush().await.unwrap();

        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let closed = FrameWriter {
            tx,
            diagnostics: FrameDiagnostics::new(),
            pending: Arc::new(AtomicUsize::new(0)),
            failure: Arc::new(Mutex::new(None)),
        };
        assert!(closed.append(record("closed".into())).await.is_err());
        assert_eq!(closed.pending.load(Ordering::Acquire), 0);
    }
    #[tokio::test]
    async fn detached_writer_keeps_the_original_storage_binding_after_switch() {
        let session = session(false);
        let old_storage = session.storage();
        let target = self::session(false);
        let target_storage = target.storage();
        let writer = FrameWriter::new(session.clone());
        let record = LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: session.id_generator().next(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            run_id: "old-run".into(),
            stream_index: 0,
            op: AssistantFrameOp::Append,
            frame: Some(serde_json::json!({})),
        });

        writer.append(record).await.unwrap();
        drop(writer);
        session.set_storage(target_storage.clone());
        tokio::task::yield_now().await;

        let old_records = old_storage.read_progress("old-run").await.unwrap();
        let target_records = target_storage.read_progress("old-run").await.unwrap();
        assert_eq!(
            old_records.len(),
            1,
            "queued work belongs to the old session"
        );
        assert_eq!(
            target_records.len(),
            0,
            "a switch must not receive old work"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn frame_checkpoint_deadline_is_not_reset_by_busy_input() {
        let session = session(false);
        let storage = session.storage();
        let writer = FrameWriter::new(session.clone());
        let record = |id: String| {
            LaneRecord::AssistantFrame(AssistantFrameRecord {
                base: RecordBase {
                    id,
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                run_id: "busy-run".into(),
                stream_index: 0,
                op: AssistantFrameOp::Append,
                frame: Some(serde_json::json!({})),
            })
        };

        writer
            .append(record(session.id_generator().next()))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(40)).await;
        writer
            .append(record(session.id_generator().next()))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        // The first frame's deadline was t=50ms. A relative timeout reset by
        // the second frame would instead wait until t=90ms and fail this check.
        tokio::time::advance(Duration::from_millis(20)).await;
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }

        let records = storage.read_progress("busy-run").await.unwrap();
        assert_eq!(
            records.len(),
            2,
            "the batch must flush at its first deadline"
        );
        drop(writer);
    }

    #[tokio::test]
    async fn harness_switch_adopts_the_target_recovery_cache_and_existing_views() {
        use crate::agent_harness::AgentHarness;
        use crate::types::AgentHarnessOptions;
        for target_volatile in [false, true] {
            let source = session(true);
            let harness = AgentHarness::create(AgentHarnessOptions {
                session: source,
                ..Default::default()
            })
            .await
            .unwrap();
            let existing_view = harness.session().clone();
            let old = FrameRecordingEmitter::new(
                Arc::new(CollectorEmitter::default()),
                existing_view.clone(),
                "main",
                "old-run",
            );
            for event in events() {
                old.emit(event).await;
            }
            old.flush().await.unwrap();
            let target = session(target_volatile);
            let recorder = FrameRecordingEmitter::new(
                Arc::new(CollectorEmitter::default()),
                target.clone(),
                "main",
                "target-run",
            );
            for event in events() {
                recorder.emit(event).await;
            }
            recorder.flush().await.unwrap();
            bind_stream_entry(&target, "main", "target-run", 0, "target-entry")
                .await
                .unwrap();
            let expected = salvage_run_frames(&target, "target-run").await.unwrap();
            assert_eq!(expected.len(), 1);
            harness.set_session(target.clone()).await.unwrap();
            assert_eq!(
                salvage_run_frames(harness.session(), "target-run")
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(
                salvage_run_frames(&existing_view, "target-run")
                    .await
                    .unwrap(),
                expected
            );
            assert!(salvage_run_frames(harness.session(), "old-run")
                .await
                .unwrap()
                .is_empty());
            target
                .storage()
                .append_entry(
                    ProvisionedEntry {
                        id: "target-entry".into(),
                        kind: ProvisionedKind::Message {
                            message: rpi_ai::UserMessage::new("committed", 42).into(),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            assert!(
                salvage_run_frames(harness.session(), "target-run")
                    .await
                    .unwrap()
                    .is_empty(),
                "the target's entry binding must survive the switch"
            );
        }
    }
    include!("frame_progress_cold_tests.rs");
}
