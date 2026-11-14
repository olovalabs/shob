//! Mirrors `packages/agent/src/harness/reducer.ts` — the record-log validator
//! that flags unrecoverable contradictions in a lane's durable slice.
//!
//! **Scope of this module (M5b):** the [`validate_record_log`] corruption
//! validator + its helpers + [`RecordLogSlice`] / [`RecordLogCorruption`] /
//! [`EffectiveLaneConfiguration`]. This is the M5b/M5g deliverable (the
//! `reducer.rs` integration test exercises the corruption reasons).
//!
//! **Deferred to M5f:** the TS `reduceLaneState` lane-reconstruction (`LaneState`,
//! `ToolBatchState`, `TerminalFailureState`, `deriveEffectiveConfiguration`,
//! `deriveToolBatch`, …) is consumed by the harness run loop when resuming a
//! suspended operation. It is ported together with the `AgentHarness` recovery
//! path rather than landing half-wired here. The validation logic below is the
//! part the recoverable-session invariants (plan §5) depend on, and it is
//! self-contained: it neither reads nor mutates [`crate::session::state::SessionState`].
//!
//! ## Corruption reasons (mirrors TS `RecordLogCorruptionReason`)
//!
//! | reason | raised when |
//! |---|---|
//! | `MultipleOpenOperations` | ≥2 unfinished `operation_started` in the slice |
//! | `UnknownOperation` | a `runId`-bearing record references an unstarted operation |
//! | `RecordAfterFinish` | an operation-owned record follows `operation_finished` (legacy queue cancellations may follow) |
//! | `NonConsecutiveAttempt` | a step attempt's number skips within its series |
//! | `InvalidCompactionReason` | a non-compaction attempt carries a reason, or vice-versa |
//! | `QueueAfterAbort` | a steer/follow-up enqueue arrives after the op aborted |
//! | `InvalidQueueCancellation` | a cancel has no pending matching enqueue |
//! | `InconsistentStep` | structural (compaction) attempts disagree on result/reason |
//! | `ToolCallMismatch` | a `tool_started` does not match its assistant tool-call ordinal |
//! | `DuplicateToolInvocation` | two `tool_started` share `(assistantEntryId, toolIndex)` |
//! | `ProvisionedEntryMismatch` | a provisioned id exists with different content |
//! | `InvalidDeferredHandle` | a deferred assistant message carries no handle |

use std::collections::{HashMap, HashSet};

use rpi_ai::types::{Content, StopReason};

use crate::session::types::{
    Entry, LaneRecord, OperationIntent, OperationStartedRecord, ProvisionedEntryJSON,
    QueueEnqueuedRecord, QueueKind, StepAttemptRecord, StepKind, ToolStartedRecord,
};

/// Machine-readable category for an unrecoverable contradiction in a lane's
/// recovery slice. Mirrors TS `RecordLogCorruptionReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordLogCorruptionReason {
    MultipleOpenOperations,
    UnknownOperation,
    RecordAfterFinish,
    NonConsecutiveAttempt,
    InvalidCompactionReason,
    QueueAfterAbort,
    InvalidQueueCancellation,
    InconsistentStep,
    ToolCallMismatch,
    DuplicateToolInvocation,
    ProvisionedEntryMismatch,
    InvalidDeferredHandle,
}

impl RecordLogCorruptionReason {
    /// Matches the TS string tags (`"multiple_open_operations"`, …) so corrupt
    /// slices round-trip the same discriminator through JSONL/logs.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordLogCorruptionReason::MultipleOpenOperations => "multiple_open_operations",
            RecordLogCorruptionReason::UnknownOperation => "unknown_operation",
            RecordLogCorruptionReason::RecordAfterFinish => "record_after_finish",
            RecordLogCorruptionReason::NonConsecutiveAttempt => "non_consecutive_attempt",
            RecordLogCorruptionReason::InvalidCompactionReason => "invalid_compaction_reason",
            RecordLogCorruptionReason::QueueAfterAbort => "queue_after_abort",
            RecordLogCorruptionReason::InvalidQueueCancellation => "invalid_queue_cancellation",
            RecordLogCorruptionReason::InconsistentStep => "inconsistent_step",
            RecordLogCorruptionReason::ToolCallMismatch => "tool_call_mismatch",
            RecordLogCorruptionReason::DuplicateToolInvocation => "duplicate_tool_invocation",
            RecordLogCorruptionReason::ProvisionedEntryMismatch => "provisioned_entry_mismatch",
            RecordLogCorruptionReason::InvalidDeferredHandle => "invalid_deferred_handle",
        }
    }
}

/// A contradiction in a lane's durable recovery slice. Restore must reject
/// rather than repair. Mirrors TS `RecordLogCorruption`.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordLogCorruption {
    pub reason: RecordLogCorruptionReason,
    pub message: String,
}

impl RecordLogCorruption {
    pub fn new(reason: RecordLogCorruptionReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RecordLogCorruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason.as_str(), self.message)
    }
}

impl std::error::Error for RecordLogCorruption {}

/// Build a corruption `Err` — the Rust analog of TS `corrupt(reason, message)`
/// which throws. Helpers use `?` against this `Result`.
fn corrupt<T>(
    reason: RecordLogCorruptionReason,
    message: impl Into<String>,
) -> Result<T, RecordLogCorruption> {
    Err(RecordLogCorruption::new(reason, message))
}

/// A bounded lane recovery slice. Mirrors TS `RecordLogSlice`.
///
/// `open_operations` are unfinished `operation_started` records (newest first);
/// `records` is the lane's record log (validator sorts a clone by `seq`);
/// `entries` are the operation-owned entries plus any referenced by provisioned
/// or result ids.
#[derive(Debug, Clone, Default)]
pub struct RecordLogSlice {
    pub lane: String,
    pub open_operations: Vec<OperationStartedRecord>,
    pub records: Vec<LaneRecord>,
    pub entries: Vec<Entry>,
}

/// Effective lane configuration derived from persisted `model_change` /
/// `thinking_level_change` / `active_tools_change` entries. Used by the deferred
/// `reduceLaneState`; defined here so the M5f harness recovery can reference it.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveLaneConfiguration {
    pub provider: String,
    pub model_id: String,
    pub thinking_level: String,
    pub active_tool_names: Vec<String>,
}

// ---- single-element series carried across attempt validation ----

struct AttemptSeries {
    record: StepAttemptRecord,
}

/// Validates a bounded lane recovery slice without reading or mutating session
/// state. Mirrors TS `validateRecordLog`. Returns `Ok(())` if the slice is a
/// legal product of the single-writer record protocol, or `Err` with the first
/// contradiction found.
///
/// The slice inputs are **not mutated** (the TS "does not mutate its bounded
/// recovery inputs" invariant): the validator clones records into a sorted
/// vec and builds a fresh entries-by-id map.
pub fn validate_record_log(input: &RecordLogSlice) -> Result<(), RecordLogCorruption> {
    if input.open_operations.len() > 1 {
        return corrupt(
            RecordLogCorruptionReason::MultipleOpenOperations,
            format!("Lane {} has at least two open operations", input.lane),
        );
    }

    let entries_by_id: HashMap<String, Entry> = input
        .entries
        .iter()
        .map(|e| (e.id().to_string(), e.clone()))
        .collect();
    validate_deferred_handles(input.entries.iter())?;

    let mut starts: HashMap<String, OperationStartedRecord> = HashMap::new();
    let mut finished_at: HashMap<String, u64> = HashMap::new();
    let mut aborted_at: HashMap<String, u64> = HashMap::new();
    let mut queue_enqueues: HashMap<String, QueueEnqueuedRecord> = HashMap::new();
    let mut latest_attempt: HashMap<String, AttemptSeries> = HashMap::new();
    let mut tool_invocations: HashSet<String> = HashSet::new();

    // Sort by shared sequence (TS: `[...input.records].sort((l,r) => l.seq - r.seq)`).
    let mut records = input.records.clone();
    records.sort_by_key(|r| r.seq());

    for record in &records {
        if let LaneRecord::OperationStarted(started) = record {
            starts.insert(started.base.id.clone(), started.clone());
            validate_operation_result(&entries_by_id, started)?;
            continue;
        }

        // `hasRunId(record)` — every non-start record with a string `runId`.
        if let Some(run_id) = record.run_id() {
            if !starts.contains_key(run_id) {
                return corrupt(
                    RecordLogCorruptionReason::UnknownOperation,
                    format!(
                        "Record {} references unknown operation {}",
                        record.id(),
                        run_id
                    ),
                );
            }
            if let Some(finish_seq) = finished_at.get(run_id) {
                // Older queue records carried the accepting run's id. Their
                // cancellation is a lane action that may happen after that run
                // ended. Keep its provenance for matching the enqueue below;
                // every other operation record still obeys the finish barrier.
                if record.seq() > *finish_seq && !matches!(record, LaneRecord::QueueCancelled(_)) {
                    return corrupt(
                        RecordLogCorruptionReason::RecordAfterFinish,
                        format!(
                            "Record {} follows the finish of operation {}",
                            record.id(),
                            run_id
                        ),
                    );
                }
            }
        }

        match record {
            LaneRecord::OperationFinished(r) => {
                finished_at.insert(r.run_id.clone(), r.base.seq);
            }
            LaneRecord::AbortRequested(r) => {
                aborted_at.insert(r.run_id.clone(), r.base.seq);
            }
            LaneRecord::StepAttempt(r) => {
                validate_attempt_reason(r)?;
                validate_attempt_sequence(r, latest_attempt.get(&r.run_id), &entries_by_id)?;
                validate_attempt_result(&entries_by_id, r)?;
                latest_attempt.insert(r.run_id.clone(), AttemptSeries { record: r.clone() });
            }
            LaneRecord::ToolStarted(r) => {
                validate_tool_start(r, &entries_by_id, &mut tool_invocations)?;
            }
            // Assistant frames are run *progress*, not branch history: they
            // reference no entries and impose no invariants on the tree, so the
            // reducer only has to accept them (recovery reads them directly via
            // `frame_progress::salvage_run_frames`).
            LaneRecord::AssistantFrame(_) => {}
            // A scheduled retry is run *progress* like the frames: it references no
            // entries and imposes no tree invariant. Recovery reads it directly to
            // decide whether to retry rather than salvage a failed attempt.
            LaneRecord::RetryPending(_) => {}
            LaneRecord::QueueEnqueued(r) => {
                // `queue !== "nextRun"` steering/follow-up after the op aborted.
                if r.queue != QueueKind::NextRun {
                    if let Some(run_id) = r.run_id.as_deref() {
                        if let Some(aborted_seq) = aborted_at.get(run_id) {
                            if r.base.seq > *aborted_seq {
                                return corrupt(
                                    RecordLogCorruptionReason::QueueAfterAbort,
                                    format!(
                                        "{} item {} was enqueued after abort",
                                        r.queue.as_str(),
                                        provisioned_id(&r.target).unwrap_or_default()
                                    ),
                                );
                            }
                        }
                    }
                }
                let target_id = provisioned_id(&r.target);
                if let Some(id) = target_id.clone() {
                    queue_enqueues.insert(id, r.clone());
                }
                validate_exact_provisioned_entry(&entries_by_id, &r.target)?;
            }
            LaneRecord::QueueCancelled(r) => {
                let enqueue = queue_enqueues.get(r.entry_id.as_str());
                let entry_exists = entries_by_id.contains_key(r.entry_id.as_str());
                let valid = match enqueue {
                    Some(enq) => {
                        enq.base.seq < r.base.seq && enq.run_id == r.run_id && !entry_exists
                    }
                    None => false,
                };
                if !valid {
                    return corrupt(
                        RecordLogCorruptionReason::InvalidQueueCancellation,
                        format!(
                            "Queue cancellation {} has no pending matching enqueue",
                            r.base.id
                        ),
                    );
                }
            }
            LaneRecord::WriteDeferred(r) => {
                validate_exact_provisioned_entry(&entries_by_id, &r.target)?;
            }
            LaneRecord::Usage(_) => { /* no cross-record checks */ }
            LaneRecord::OperationStarted(_) => { /* handled above */ }
        }
    }

    Ok(())
}

// ---- validation helpers ----

/// `matchesProvisionedEntry(entry, target)`: drop `parentId`/`seq`/`timestamp`
/// from the entry's flat JSON and deep-compare to the provisioned target.
pub(crate) fn matches_provisioned_entry(entry: &Entry, target: &serde_json::Value) -> bool {
    entry_provisioned_json(entry) == *target
}

/// Build the provisioned JSON view of an entry — the flat entry shape *minus*
/// the storage-assigned `seq`/`parentId`/`timestamp` (mirrors TS
/// `Omit<Entry, "parentId"|"seq"|"timestamp">`). Reuses [`Entry::to_flat_json`]
/// (which already omits `None` optionals via serde) then strips the three
/// storage keys.
pub(crate) fn entry_provisioned_json(entry: &Entry) -> serde_json::Value {
    let mut value = entry.to_flat_json();
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("seq");
        map.remove("parentId");
        map.remove("timestamp");
    }
    value
}

fn provisioned_id(target: &ProvisionedEntryJSON) -> Option<String> {
    target.get("id").and_then(|v| v.as_str()).map(String::from)
}

/// `validateExactProvisionedEntry`: if a provisioned id is already persisted, it
/// must have identical content — otherwise the intent and the durable log
/// disagree.
fn validate_exact_provisioned_entry(
    entries_by_id: &HashMap<String, Entry>,
    target: &ProvisionedEntryJSON,
) -> Result<(), RecordLogCorruption> {
    if let Some(id) = provisioned_id(target) {
        if let Some(entry) = entries_by_id.get(&id) {
            if !matches_provisioned_entry(entry, target) {
                return corrupt(
                    RecordLogCorruptionReason::ProvisionedEntryMismatch,
                    format!("Provisioned entry {id} exists with content different from its intent"),
                );
            }
        }
    }
    Ok(())
}

/// `validateResultEntry`: if the result entry exists, it must satisfy `matches`.
fn validate_result_entry(
    entries_by_id: &HashMap<String, Entry>,
    result_entry_id: &str,
    matches: impl Fn(&Entry) -> bool,
    description: &str,
) -> Result<(), RecordLogCorruption> {
    if let Some(entry) = entries_by_id.get(result_entry_id) {
        if !matches(entry) {
            return corrupt(
                RecordLogCorruptionReason::ProvisionedEntryMismatch,
                format!(
                    "Provisioned {} entry {result_entry_id} exists with different content",
                    description
                ),
            );
        }
    }
    Ok(())
}

/// `validateAttemptReason`: compaction steps require a reason in
/// {manual,threshold,overflow}; non-compaction steps must carry none.
fn validate_attempt_reason(record: &StepAttemptRecord) -> Result<(), RecordLogCorruption> {
    match record.step {
        StepKind::Compaction => {
            if record.compaction_reason.is_none() {
                return corrupt(
                    RecordLogCorruptionReason::InvalidCompactionReason,
                    format!(
                        "Compaction attempt {} has no valid compaction reason",
                        record.base.id
                    ),
                );
            }
        }
        _ => {
            if record.compaction_reason.is_some() {
                return corrupt(
                    RecordLogCorruptionReason::InvalidCompactionReason,
                    format!(
                        "{:?} attempt {} has a compaction reason",
                        record.step, record.base.id
                    ),
                );
            }
        }
    }
    Ok(())
}

/// `validateAttemptSequence`: a step's `attempt` number either starts a series
/// (1) or continues the previous same-step attempt (+1). A series continues only
/// when the previous attempt's result entry is *not yet persisted* in the slice
/// — `previousResult === undefined` — OR the result was persisted at a seq at or
/// after this record (`previousResult.seq >= record.seq`, the retry case where
/// the shared result lands after both attempts).
fn validate_attempt_sequence(
    record: &StepAttemptRecord,
    previous: Option<&AttemptSeries>,
    entries_by_id: &HashMap<String, Entry>,
) -> Result<(), RecordLogCorruption> {
    let previous_record = previous.map(|s| &s.record);
    let previous_result = previous_record.and_then(|p| entries_by_id.get(&p.result_entry_id));

    let continues_series = match previous_record {
        Some(prev) => {
            prev.step == record.step
                && match previous_result {
                    None => true,
                    Some(r) => r.seq() >= record.base.seq,
                }
        }
        None => false,
    };
    let expected_attempt = if continues_series {
        previous_record.unwrap().attempt + 1
    } else {
        1
    };
    if record.attempt != expected_attempt {
        return corrupt(
            RecordLogCorruptionReason::NonConsecutiveAttempt,
            format!(
                "{:?} attempt {} is {}; expected {}",
                record.step, record.base.id, record.attempt, expected_attempt
            ),
        );
    }

    // Structural consistency only across continuing non-assistant series.
    if !continues_series || record.step == StepKind::Assistant {
        return Ok(());
    }
    let prev = previous_record.unwrap();
    if record.result_entry_id != prev.result_entry_id {
        return corrupt(
            RecordLogCorruptionReason::InconsistentStep,
            format!(
                "{:?} attempts disagree on their result entry id",
                record.step
            ),
        );
    }
    if record.compaction_reason != prev.compaction_reason {
        return corrupt(
            RecordLogCorruptionReason::InconsistentStep,
            format!(
                "{:?} attempts disagree on their compaction reason",
                record.step
            ),
        );
    }
    Ok(())
}

/// `validateAttemptResult`: the persisted result entry (if any) must match the
/// step kind.
fn validate_attempt_result(
    entries_by_id: &HashMap<String, Entry>,
    record: &StepAttemptRecord,
) -> Result<(), RecordLogCorruption> {
    match record.step {
        StepKind::Assistant => validate_result_entry(
            entries_by_id,
            &record.result_entry_id,
            is_assistant_message,
            "assistant result",
        ),
        StepKind::Compaction => validate_result_entry(
            entries_by_id,
            &record.result_entry_id,
            |e| matches!(e, Entry::Compaction(_)),
            "compaction result",
        ),
        StepKind::BranchSummary => validate_result_entry(
            entries_by_id,
            &record.result_entry_id,
            |e| matches!(e, Entry::BranchSummary(_)),
            "branch-summary result",
        ),
    }
}

fn is_assistant_message(entry: &Entry) -> bool {
    matches!(entry, Entry::Message(m) if m.message.is_assistant())
}

/// `validateToolStart`: no duplicate `(assistantEntryId, toolIndex)`, the
/// referenced assistant entry must carry that tool-call ordinal, and the result
/// entry (if any) must be the matching toolResult.
fn validate_tool_start(
    record: &ToolStartedRecord,
    entries_by_id: &HashMap<String, Entry>,
    invocations: &mut HashSet<String>,
) -> Result<(), RecordLogCorruption> {
    let mut key = record.assistant_entry_id.clone();
    key.push('\u{0}');
    key.push_str(&record.tool_index.to_string());
    if !invocations.insert(key) {
        return corrupt(
            RecordLogCorruptionReason::DuplicateToolInvocation,
            format!(
                "Tool invocation {}:{} is duplicated",
                record.assistant_entry_id, record.tool_index
            ),
        );
    }

    let assistant_entry = entries_by_id.get(&record.assistant_entry_id);
    let tool_call = match assistant_entry {
        Some(Entry::Message(m)) if m.message.is_assistant() => {
            let asst = match &m.message {
                rpi_agent::message::AgentMessage::Assistant(a) => Some(a.as_ref()),
                _ => None,
            };
            match asst {
                Some(a) => a
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::ToolCall(t) => Some(t),
                        _ => None,
                    })
                    .nth(record.tool_index as usize),
                None => None,
            }
        }
        _ => None,
    };
    let matches_ordinal = match tool_call {
        Some(tc) => tc.id == record.tool_call_id && tc.name == record.tool_name,
        None => false,
    };
    if !matches_ordinal {
        return corrupt(
            RecordLogCorruptionReason::ToolCallMismatch,
            format!(
                "Tool start {} does not match its assistant tool-call ordinal",
                record.base.id
            ),
        );
    }

    validate_result_entry(
        entries_by_id,
        &record.result_entry_id,
        |e| is_tool_result_for(e, &record.tool_call_id, &record.tool_name),
        "tool result",
    )
}

fn is_tool_result_for(entry: &Entry, tool_call_id: &str, tool_name: &str) -> bool {
    match entry {
        Entry::Message(m) => match &m.message {
            rpi_agent::message::AgentMessage::ToolResult(tr) => {
                tr.tool_call_id == tool_call_id && tr.tool_name == tool_name
            }
            _ => false,
        },
        _ => false,
    }
}

/// `validateDeferredHandles`: a deferred-stop assistant entry must carry a handle.
fn validate_deferred_handles<'a>(
    entries: impl Iterator<Item = &'a Entry>,
) -> Result<(), RecordLogCorruption> {
    for entry in entries {
        if let Entry::Message(m) = entry {
            if let rpi_agent::message::AgentMessage::Assistant(asst) = &m.message {
                if asst.stop_reason == StopReason::Deferred && asst.deferred.is_none() {
                    return corrupt(
                        RecordLogCorruptionReason::InvalidDeferredHandle,
                        format!(
                            "Deferred assistant entry {} does not carry a handle",
                            entry.id()
                        ),
                    );
                }
            }
        }
    }
    Ok(())
}

/// `validateOperationResult`: per-intent result validation. Run intents check
/// every initial message's provisioned content; compaction intents check the
/// result entry type; navigation intents check the summary entry type.
fn validate_operation_result(
    entries_by_id: &HashMap<String, Entry>,
    record: &OperationStartedRecord,
) -> Result<(), RecordLogCorruption> {
    match &record.intent {
        OperationIntent::Run {
            initial_messages, ..
        } => {
            for target in initial_messages {
                validate_exact_provisioned_entry(entries_by_id, target)?;
            }
        }
        OperationIntent::Compaction {
            result_entry_id, ..
        } => {
            validate_result_entry(
                entries_by_id,
                result_entry_id,
                |e| matches!(e, Entry::Compaction(_)),
                "manual compaction",
            )?;
        }
        OperationIntent::Navigation {
            summary_entry_id: Some(summary_id),
            ..
        } => {
            validate_result_entry(
                entries_by_id,
                summary_id,
                |e| matches!(e, Entry::BranchSummary(_)),
                "navigation summary",
            )?;
        }
        OperationIntent::Navigation {
            summary_entry_id: None,
            ..
        } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slice_is_valid() {
        let slice = RecordLogSlice {
            lane: "main".to_string(),
            ..Default::default()
        };
        validate_record_log(&slice).expect("empty slice is valid");
    }
}
