//! Durable operation runtime: admission, recovery, reconcile, checkpoint.
//!
//! Mirrors the role of `packages/agent/src/harness/runtime/` in the TS tree.
//! The Rust harness already persists the primitives this layer reasons over —
//! `operation_started` / `operation_finished` records, `write_deferred`
//! provisioned frames, `tool_started` frames, and the append-only sequence —
//! so the runtime is a *reader + decision* layer, not a second store.
//!
//! ## Why it exists
//!
//! A process can die between "operation_started" and "operation_finished"
//! (crash, `kill -9`, power loss). On restart the session is still valid (the
//! log is append-only and torn tails are dropped), but the lane is left with an
//! operation nobody is driving and possibly a half-committed frame. Recovery
//! has to answer three questions per lane:
//!
//! 1. Which operations are still open? ([`SessionRuntime::recover_lane`])
//! 2. Is that state legal, or corrupt? ([`LaneRecovery::findings`])
//! 3. What should the caller do about each? ([`SessionRuntime::reconcile`])
//!
//! Recovery **never mutates** the session. It returns a report plus decisions;
//! acting on them (abort, resume, skip) stays with the caller, so a failed
//! recovery cannot itself corrupt the log.
//!
//! ## Admission
//!
//! [`SessionRuntime::admit`] is the inverse check: before starting a new
//! operation on a lane, it refuses when an operation is already open. That is
//! the harness-level "at-most-one open op per lane" precondition made explicit
//! and reusable.

use std::collections::{BTreeMap, BTreeSet};

use rpi_agent::message::AgentMessage;

use crate::error::{SessionError, SessionResult};
use crate::events::{HarnessEvent, HarnessEventBus};
use crate::session::session::Session;
use crate::session::types::{Entry, EntryOrder, LaneRecord, OperationIntent, RecordQuery};

/// How many open operations constitute corruption (mirrors the TS
/// `find_open_operations(limit: 2)` rule).
const CORRUPTION_OPEN_OP_THRESHOLD: usize = 2;

/// A lane operation left open by a previous process.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOperation {
    pub run_id: String,
    pub lane: String,
    pub source_leaf_id: Option<String>,
    pub intent: OperationIntent,
    pub started_at: i64,
}

impl OpenOperation {
    /// The TS `intent.kind` string (`run` / `compaction` / `navigation`).
    pub fn kind(&self) -> &'static str {
        self.intent.kind()
    }
}

/// A `write_deferred` frame: an entry provisioned by an open operation whose
/// final content has not been committed yet.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingWrite {
    pub run_id: String,
    pub target: serde_json::Value,
    pub provisioned_id: Option<String>,
}

/// A `tool_started` frame: a tool call whose result entry may or may not exist.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolFrame {
    pub run_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub result_entry_id: String,
    /// The arguments the call was recorded with. Recovery needs them to re-run a
    /// replayable call: the in-memory arguments died with the process, and the
    /// recorded ones are the only faithful copy.
    pub effective_args: serde_json::Value,
    /// The replay policy recorded when the call started. Recovery requires this
    /// **and** the current tool declaration to say `Safe` before re-running.
    pub replay: crate::session::types::ToolReplay,
}

/// A structural problem found while reconciling a lane.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryFinding {
    /// More than one operation is open on the lane — the log is corrupt
    /// (the reducer's `multiple_open_operations` check).
    MultipleOpenOperations { lane: String, run_ids: Vec<String> },
    /// An open operation has no driver. Without a deferred handle to resume,
    /// the only safe resolution is to abort it.
    OrphanedOperation { run_id: String },
    /// A deferred write exists for a run with no matching open operation.
    DanglingWrite { run_id: String },
    /// A tool frame exists for a run with no matching open operation.
    DanglingToolFrame {
        run_id: String,
        tool_call_id: String,
    },
}

impl RecoveryFinding {
    pub fn code(&self) -> &'static str {
        match self {
            RecoveryFinding::MultipleOpenOperations { .. } => "multiple_open_operations",
            RecoveryFinding::OrphanedOperation { .. } => "orphaned_operation",
            RecoveryFinding::DanglingWrite { .. } => "dangling_write",
            RecoveryFinding::DanglingToolFrame { .. } => "dangling_tool_frame",
        }
    }
}

/// The reconciled state of one lane after recovery. Read-only.
#[derive(Debug, Clone, PartialEq)]
pub struct LaneRecovery {
    pub lane: String,
    pub leaf_id: Option<String>,
    pub open_operations: Vec<OpenOperation>,
    pub pending_writes: Vec<PendingWrite>,
    pub tool_frames: Vec<ToolFrame>,
    pub findings: Vec<RecoveryFinding>,
}

impl LaneRecovery {
    /// Whether the lane needs any recovery action at all.
    pub fn is_clean(&self) -> bool {
        self.open_operations.is_empty()
            && self.pending_writes.is_empty()
            && self.findings.is_empty()
    }

    /// Whether a structural corruption was detected (as opposed to ordinary
    /// interrupted work).
    pub fn is_corrupt(&self) -> bool {
        self.findings
            .iter()
            .any(|f| matches!(f, RecoveryFinding::MultipleOpenOperations { .. }))
    }

    /// The single open operation, when exactly one exists.
    pub fn sole_open_operation(&self) -> Option<&OpenOperation> {
        match self.open_operations.as_slice() {
            [one] => Some(one),
            _ => None,
        }
    }
}

/// What the caller should do about one recovered operation.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryDecision {
    /// Abort the orphaned operation and settle the lane.
    Abort { run_id: String },
    /// The lane is structurally corrupt — surface it, do not auto-repair.
    FlagCorruption { lane: String, run_ids: Vec<String> },
    /// Committed frames for a run with no open operation — informational.
    DropDangling { run_id: String },
}

impl RecoveryDecision {
    pub fn run_id(&self) -> Option<&str> {
        match self {
            RecoveryDecision::Abort { run_id } => Some(run_id),
            RecoveryDecision::FlagCorruption { .. } => None,
            RecoveryDecision::DropDangling { run_id } => Some(run_id),
        }
    }
}

/// A point-in-time progress marker for a lane, used by the TUI/CLI to detect
/// "did anything change since I last looked".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneCheckpoint {
    pub lane: String,
    pub leaf_id: Option<String>,
    /// Highest entry sequence observed on the lane.
    pub last_entry_seq: u64,
    /// Highest record sequence observed on the lane.
    pub last_record_seq: u64,
    /// Whether an operation is currently open.
    pub has_open_operation: bool,
}

impl LaneCheckpoint {
    /// Whether `self` is strictly behind `other` — i.e. `other` observed more
    /// progress on the log. Progress is ordered by `(record_seq, entry_seq)`;
    /// opening/closing an operation always appends a record, so the sequence
    /// comparison alone captures it.
    pub fn is_behind(&self, other: &LaneCheckpoint) -> bool {
        (self.last_record_seq, self.last_entry_seq) < (other.last_record_seq, other.last_entry_seq)
    }
}

/// The runtime facade over a session. Cheap to clone (holds a `Session`).
#[derive(Clone)]
pub struct SessionRuntime {
    session: Session,
}

impl SessionRuntime {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    /// `admit(lane)` — the precondition for starting a new operation: refuse
    /// when one is already open. Mirrors the TS admission check.
    pub async fn admit(&self, lane: &str) -> SessionResult<()> {
        let open = self.session.find_open_operations(lane, Some(1)).await?;
        if let Some(op) = open.first() {
            return Err(SessionError::invalid_lane(format!(
                "lane '{lane}' already has an open operation ({})",
                op.base.id
            )));
        }
        Ok(())
    }

    /// `recoverLane(lane)` — read-only recovery report for one lane.
    pub async fn recover_lane(&self, lane: &str) -> SessionResult<LaneRecovery> {
        let leaf_id = self.session.view(lane).get_leaf_id().await.ok().flatten();

        let open_records = self.session.find_open_operations(lane, None).await?;
        let open_ids: BTreeSet<String> = open_records.iter().map(|r| r.base.id.clone()).collect();
        let open_operations: Vec<OpenOperation> = open_records
            .into_iter()
            .map(|record| OpenOperation {
                run_id: record.base.id.clone(),
                lane: record.base.lane.clone(),
                source_leaf_id: record.source_leaf_id.clone(),
                intent: record.intent.clone(),
                started_at: record.base.timestamp,
            })
            .collect();

        // Only these record types contribute to the recovery report. In
        // particular, reporting recovery must not hydrate retired stream frames.
        let mut records = Vec::new();
        for record_type in ["write_deferred", "tool_started"] {
            records.extend(
                self.session
                    .find_records(&RecordQuery {
                        lane: Some(lane.to_string()),
                        record_type: Some(record_type),
                        order: Some(EntryOrder::OldestFirst),
                        ..Default::default()
                    })
                    .await?,
            );
        }
        records.sort_unstable_by_key(LaneRecord::seq);

        let mut pending_writes = Vec::new();
        let mut tool_frames = Vec::new();
        let mut findings = Vec::new();

        // Whether a deferred write or a tool frame is *settled* is a property of
        // the entries, not of whether its run is still open: a run that
        // completed normally also has no open operation. Classifying on
        // `open_ids` alone flagged every settled frame of every successful run,
        // which made `is_clean()` permanently false and buried the real signal
        // (a frame that never landed because the process died).
        //
        // Settled means the referenced entry exists: for a deferred write, the
        // provisioned id was committed; for a tool frame, its reserved result
        // entry must carry the matching call ID AND tool name. Provider IDs
        // alone may recur in another batch, branch or lane.
        // Look up each reserved ID session-wide, not on the current lane or
        // branch. Hydrate at most one entry at a time; unrelated payloads (and
        // lanes with no recovery records) must not be read.

        for record in records {
            match record {
                LaneRecord::WriteDeferred(w) => {
                    let provisioned_id = w
                        .target
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    // The commit landed: this write is history, not a pending
                    // frame, whatever happened to the operation afterwards.
                    if let Some(id) = provisioned_id.as_deref() {
                        if self.session.get_entry(id).await?.is_some() {
                            continue;
                        }
                    }
                    if !open_ids.contains(&w.run_id) {
                        findings.push(RecoveryFinding::DanglingWrite {
                            run_id: w.run_id.clone(),
                        });
                    }
                    pending_writes.push(PendingWrite {
                        run_id: w.run_id,
                        target: w.target,
                        provisioned_id,
                    });
                }
                LaneRecord::ToolStarted(t) => {
                    // The tool's result landed: the invocation is settled.
                    let landed = match self.session.get_entry(&t.result_entry_id).await? {
                        Some(Entry::Message(message)) => match message.message {
                            AgentMessage::ToolResult(result) => {
                                result.tool_call_id == t.tool_call_id
                                    && result.tool_name == t.tool_name
                            }
                            _ => false,
                        },
                        _ => false,
                    };
                    if landed {
                        continue;
                    }
                    if !open_ids.contains(&t.run_id) {
                        findings.push(RecoveryFinding::DanglingToolFrame {
                            run_id: t.run_id.clone(),
                            tool_call_id: t.tool_call_id.clone(),
                        });
                    }
                    tool_frames.push(ToolFrame {
                        run_id: t.run_id,
                        tool_call_id: t.tool_call_id,
                        tool_name: t.tool_name,
                        result_entry_id: t.result_entry_id,
                        effective_args: t.effective_args,
                        replay: t.replay,
                    });
                }
                _ => {}
            }
        }

        if open_operations.len() >= CORRUPTION_OPEN_OP_THRESHOLD {
            findings.push(RecoveryFinding::MultipleOpenOperations {
                lane: lane.to_string(),
                run_ids: open_operations.iter().map(|o| o.run_id.clone()).collect(),
            });
        } else if let Some(one) = open_operations.first() {
            findings.push(RecoveryFinding::OrphanedOperation {
                run_id: one.run_id.clone(),
            });
        }

        Ok(LaneRecovery {
            lane: lane.to_string(),
            leaf_id,
            open_operations,
            pending_writes,
            tool_frames,
            findings,
        })
    }

    /// `reconcile(recovery)` — turn a recovery report into decisions. Pure: no
    /// session reads, no mutation.
    pub fn reconcile(&self, recovery: &LaneRecovery) -> Vec<RecoveryDecision> {
        let mut decisions = Vec::new();
        if recovery.is_corrupt() {
            decisions.push(RecoveryDecision::FlagCorruption {
                lane: recovery.lane.clone(),
                run_ids: recovery
                    .open_operations
                    .iter()
                    .map(|o| o.run_id.clone())
                    .collect(),
            });
            return decisions;
        }
        for op in &recovery.open_operations {
            decisions.push(RecoveryDecision::Abort {
                run_id: op.run_id.clone(),
            });
        }
        for finding in &recovery.findings {
            if let RecoveryFinding::DanglingWrite { run_id }
            | RecoveryFinding::DanglingToolFrame { run_id, .. } = finding
            {
                if !decisions.iter().any(
                    |d| matches!(d, RecoveryDecision::DropDangling { run_id: r } if r == run_id),
                ) {
                    decisions.push(RecoveryDecision::DropDangling {
                        run_id: run_id.clone(),
                    });
                }
            }
        }
        decisions
    }

    /// `checkpoint(lane)` — capture a progress marker.
    pub async fn checkpoint(&self, lane: &str) -> SessionResult<LaneCheckpoint> {
        let leaf_id = self.session.view(lane).get_leaf_id().await.ok().flatten();
        let entries = self
            .session
            .view(lane)
            .find_entries(&crate::session::types::EntryQuery {
                order: Some(EntryOrder::NewestFirst),
                limit: Some(1),
                ..Default::default()
            })
            .await?;
        let last_entry_seq = entries.first().map(|e| e.seq()).unwrap_or(0);
        let records = self
            .session
            .find_records(&RecordQuery {
                lane: Some(lane.to_string()),
                order: Some(EntryOrder::NewestFirst),
                limit: Some(1),
                ..Default::default()
            })
            .await?;
        let last_record_seq = records.first().map(|r| r.seq()).unwrap_or(0);
        let has_open_operation = !self
            .session
            .find_open_operations(lane, Some(1))
            .await?
            .is_empty();
        Ok(LaneCheckpoint {
            lane: lane.to_string(),
            leaf_id,
            last_entry_seq,
            last_record_seq,
            has_open_operation,
        })
    }

    /// Emit the recovery findings onto the harness event bus so a UI can show
    /// "recovered an interrupted run" without polling the session.
    pub fn emit_recovery_events(&self, bus: &HarnessEventBus, recovery: &LaneRecovery) {
        for op in &recovery.open_operations {
            bus.emit(&HarnessEvent::RunStart(crate::events::RunStartEvent {
                lane: recovery.lane.clone(),
                run_id: op.run_id.clone(),
            }));
            bus.emit(&HarnessEvent::RunEnd(crate::events::RunEndEvent {
                lane: recovery.lane.clone(),
                run_id: op.run_id.clone(),
                outcome: crate::events::RunEndOutcome::Aborted,
                leaf_id: recovery.leaf_id.clone().unwrap_or_default(),
            }));
        }
    }
}

/// Convenience: run recovery for every known lane. Mirrors the TS runtime's
/// startup sweep.
pub async fn recover_all_lanes(
    runtime: &SessionRuntime,
) -> SessionResult<BTreeMap<String, LaneRecovery>> {
    let lanes = runtime.session.get_lanes().await?;
    let mut out = BTreeMap::new();
    for pointer in lanes {
        let report = runtime.recover_lane(&pointer.lane).await?;
        out.insert(pointer.lane.clone(), report);
    }
    Ok(out)
}

#[cfg(test)]
#[path = "runtime_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::memory::{InMemorySessionStorage, SystemClock};
    use crate::session::session::{DefaultIdGenerator, Session};
    use crate::session::types::{
        LaneRecord, OperationFinishedRecord, OperationOutcome, OperationStartedRecord,
        ProvisionedEntry, ProvisionedEntryJSON, ProvisionedKind, RecordBase, ToolReplay,
        ToolStartedRecord, WriteDeferredRecord,
    };
    use rpi_ai::types::{ToolResultMessage, ToolResultRole};
    use std::sync::Arc;

    /// An assistant message whose only content is one tool call, so a
    /// `tool_started` record can reference it by ordinal.
    fn assistant_with_tool_call(call_id: &str, name: &str) -> rpi_ai::types::AssistantMessage {
        let mut message =
            rpi_ai::types::AssistantMessage::empty(rpi_ai::types::Api::Faux, "faux", "faux", 0);
        message
            .content
            .push(rpi_ai::types::Content::ToolCall(rpi_ai::types::ToolCall {
                kind: rpi_ai::types::ToolCallType,
                id: call_id.to_string(),
                name: name.to_string(),
                arguments: serde_json::json!({ "path": "a.txt" }),
                thought_signature: None,
                namespace: None,
                recorded: None,
            }));
        message
    }

    fn tool_result(call_id: &str, name: &str) -> ToolResultMessage {
        ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: call_id.to_string(),
            tool_name: name.to_string(),
            content: Vec::new(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 0,
        }
    }

    async fn finish_operation(session: &Session, run_id: &str, lane: &str) {
        session
            .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                base: record_base(&format!("{run_id}-fin"), lane),
                run_id: run_id.to_string(),
                outcome: OperationOutcome::Completed,
                error: None,
                result: None,
            }))
            .await
            .unwrap();
    }

    fn session() -> Session {
        let metadata = crate::session::types::SessionMetadata {
            id: "s-runtime".to_string(),
            created_at: 0,
            parent_session_id: None,
        };
        let storage = InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        );
        Session::new(Arc::new(storage), None)
    }

    fn record_base(id: &str, lane: &str) -> RecordBase {
        RecordBase {
            id: id.to_string(),
            seq: 0,
            lane: lane.to_string(),
            timestamp: 0,
        }
    }

    async fn open_operation(session: &Session, run_id: &str, lane: &str) {
        let record = LaneRecord::OperationStarted(OperationStartedRecord {
            base: record_base(run_id, lane),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: None,
            },
        });
        session.append_record(record).await.unwrap();
    }

    #[tokio::test]
    async fn clean_lane_recovers_to_empty_report() {
        let session = session();
        let runtime = SessionRuntime::new(session);
        let report = runtime.recover_lane("main").await.unwrap();
        assert!(report.is_clean());
        assert!(!report.is_corrupt());
        assert!(runtime.reconcile(&report).is_empty());
    }

    #[tokio::test]
    async fn open_operation_is_orphaned_and_reconciled_to_abort() {
        let session = session();
        open_operation(&session, "run-1", "main").await;
        let runtime = SessionRuntime::new(session);

        let report = runtime.recover_lane("main").await.unwrap();
        assert_eq!(report.open_operations.len(), 1);
        assert!(!report.is_clean());
        assert!(!report.is_corrupt());
        assert_eq!(
            report.sole_open_operation().map(|o| o.run_id.as_str()),
            Some("run-1")
        );
        assert!(report.findings.iter().any(
            |f| matches!(f, RecoveryFinding::OrphanedOperation { run_id } if run_id == "run-1")
        ));

        let decisions = runtime.reconcile(&report);
        assert_eq!(
            decisions,
            vec![RecoveryDecision::Abort {
                run_id: "run-1".to_string()
            }]
        );
    }

    #[tokio::test]
    async fn admit_rejects_when_an_operation_is_open() {
        let session = session();
        let runtime = SessionRuntime::new(session);
        runtime.admit("main").await.unwrap();

        open_operation(&runtime.session, "run-1", "main").await;
        assert!(runtime.admit("main").await.is_err());
    }

    #[tokio::test]
    async fn two_open_operations_are_reported_as_corruption() {
        // The storage layer itself enforces at-most-one open operation per lane
        // (see `admit_rejects_when_an_operation_is_open`). `MultipleOpenOperations`
        // is therefore the *defensive* reader-side check for a log that was
        // corrupted out-of-band; exercise it against a hand-built report.
        let session = session();
        let runtime = SessionRuntime::new(session);
        let report = LaneRecovery {
            lane: "main".to_string(),
            leaf_id: None,
            open_operations: vec![
                OpenOperation {
                    run_id: "run-1".to_string(),
                    lane: "main".to_string(),
                    source_leaf_id: None,
                    intent: OperationIntent::Run {
                        original_prompt: Vec::new(),
                        initial_messages: Vec::new(),
                        system_prompt_override: None,
                        resume_data: None,
                    },
                    started_at: 0,
                },
                OpenOperation {
                    run_id: "run-2".to_string(),
                    lane: "main".to_string(),
                    source_leaf_id: None,
                    intent: OperationIntent::Run {
                        original_prompt: Vec::new(),
                        initial_messages: Vec::new(),
                        system_prompt_override: None,
                        resume_data: None,
                    },
                    started_at: 0,
                },
            ],
            pending_writes: Vec::new(),
            tool_frames: Vec::new(),
            findings: vec![RecoveryFinding::MultipleOpenOperations {
                lane: "main".to_string(),
                run_ids: vec!["run-1".to_string(), "run-2".to_string()],
            }],
        };
        assert!(report.is_corrupt());
        let decisions = runtime.reconcile(&report);
        assert!(matches!(
            decisions.as_slice(),
            [RecoveryDecision::FlagCorruption { .. }]
        ));
    }

    #[tokio::test]
    async fn dangling_write_is_flagged_and_dropped() {
        let session = session();
        let target: ProvisionedEntryJSON = serde_json::json!({ "id": "prov-1" });
        session
            .append_record(LaneRecord::WriteDeferred(WriteDeferredRecord {
                base: record_base("wd-1", "main"),
                run_id: "ghost-run".to_string(),
                target,
            }))
            .await
            .unwrap();
        let runtime = SessionRuntime::new(session);

        let report = runtime.recover_lane("main").await.unwrap();
        assert_eq!(report.pending_writes.len(), 1);
        assert_eq!(
            report.pending_writes[0].provisioned_id.as_deref(),
            Some("prov-1")
        );
        assert!(report.findings.iter().any(
            |f| matches!(f, RecoveryFinding::DanglingWrite { run_id } if run_id == "ghost-run")
        ));
        assert_eq!(
            runtime.reconcile(&report),
            vec![RecoveryDecision::DropDangling {
                run_id: "ghost-run".to_string()
            }]
        );
    }

    /// A settled deferred write must not be flagged once its run has closed.
    ///
    /// Regression: the old check classified on "is the run still open", so a run
    /// that completed normally also had no open operation and every one of its
    /// settled frames was reported as dangling — `is_clean()` could never be
    /// true for a healthy session, and the real signal (a frame that never
    /// landed because the process died) was buried in the noise.
    #[tokio::test]
    async fn a_settled_write_is_not_flagged_when_its_run_closed() {
        let session = session();
        let target: ProvisionedEntryJSON = serde_json::json!({
            "id": "prov-1",
            "type": "custom",
            "customType": "note",
        });
        session
            .append_record(LaneRecord::WriteDeferred(WriteDeferredRecord {
                base: record_base("wd-1", "main"),
                run_id: "run-1".to_string(),
                target: target.clone(),
            }))
            .await
            .unwrap();
        // The commit landed: the provisioned entry exists.
        session
            .append_entry(
                ProvisionedEntry {
                    id: "prov-1".to_string(),
                    kind: ProvisionedKind::Custom {
                        custom_type: "note".to_string(),
                        data: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        // The run then closed normally.
        finish_operation(&session, "run-1", "main").await;

        let runtime = SessionRuntime::new(session);
        let report = runtime.recover_lane("main").await.unwrap();
        assert!(
            report.pending_writes.is_empty(),
            "a committed write is not pending: {:?}",
            report.pending_writes
        );
        assert!(
            !report
                .findings
                .iter()
                .any(|f| matches!(f, RecoveryFinding::DanglingWrite { .. })),
            "a committed write must not be reported dangling: {:?}",
            report.findings
        );
        assert!(
            report.is_clean(),
            "a healthy completed session must recover clean: {report:?}"
        );
    }

    /// The same rule for tool frames: a tool whose result landed is settled.
    #[tokio::test]
    async fn a_settled_tool_frame_is_not_flagged_when_its_run_closed() {
        let session = session();
        let assistant_id = session
            .view("main")
            .append_message(AgentMessage::Assistant(Box::new(assistant_with_tool_call(
                "call-1", "write",
            ))))
            .await
            .unwrap();
        session
            .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
                base: record_base("ts-1", "main"),
                run_id: "run-1".to_string(),
                assistant_entry_id: assistant_id,
                tool_index: 0,
                tool_call_id: "call-1".to_string(),
                tool_name: "write".to_string(),
                effective_args: serde_json::json!({ "path": "a.txt" }),
                result_entry_id: "result-1".to_string(),
                replay: ToolReplay::Never,
            }))
            .await
            .unwrap();
        // The tool's result landed under the reserved identity, then the run closed.
        session
            .append_entry(
                ProvisionedEntry {
                    id: "result-1".to_string(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::ToolResult(Box::new(tool_result("call-1", "write"))),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        finish_operation(&session, "run-1", "main").await;

        let runtime = SessionRuntime::new(session);
        let report = runtime.recover_lane("main").await.unwrap();
        assert!(
            report.tool_frames.is_empty(),
            "a tool whose result landed is not pending: {:?}",
            report.tool_frames
        );
        assert!(
            !report
                .findings
                .iter()
                .any(|f| matches!(f, RecoveryFinding::DanglingToolFrame { .. })),
            "a settled tool frame must not be reported dangling: {:?}",
            report.findings
        );
        assert!(report.is_clean(), "healthy session: {report:?}");
    }

    /// A tool frame whose result never landed, for a run that is closed, is the
    /// real crash signal — and it names the tool so a caller can tell the user
    /// which side effect is unknown.
    #[tokio::test]
    async fn an_unlanded_tool_frame_names_the_tool() {
        let session = session();
        let assistant_id = session
            .view("main")
            .append_message(AgentMessage::Assistant(Box::new(assistant_with_tool_call(
                "call-1", "write",
            ))))
            .await
            .unwrap();
        session
            .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
                base: record_base("ts-1", "main"),
                run_id: "run-1".to_string(),
                assistant_entry_id: assistant_id,
                tool_index: 0,
                tool_call_id: "call-1".to_string(),
                tool_name: "write".to_string(),
                effective_args: serde_json::json!({ "path": "a.txt" }),
                result_entry_id: "result-1".to_string(),
                replay: ToolReplay::Never,
            }))
            .await
            .unwrap();
        // No result was ever appended; the run then closed (crash recovery
        // closes the operation).
        finish_operation(&session, "run-1", "main").await;

        let runtime = SessionRuntime::new(session);
        let report = runtime.recover_lane("main").await.unwrap();
        assert_eq!(report.tool_frames.len(), 1);
        assert_eq!(
            report.tool_frames[0].tool_name, "write",
            "recovery must know which tool's outcome is unknown"
        );
        assert!(report.findings.iter().any(|f| matches!(
            f,
            RecoveryFinding::DanglingToolFrame { tool_call_id, .. } if tool_call_id == "call-1"
        )));
        assert!(!report.is_clean());
        assert!(
            !report.is_corrupt(),
            "an interrupted tool is not corruption"
        );
    }

    /// An in-flight tool frame on a still-open run is ordinary progress, not a
    /// finding: the doctor should only speak up about a run that died.
    #[tokio::test]
    async fn an_in_flight_tool_frame_is_not_a_finding() {
        let session = session();
        let assistant_id = session
            .view("main")
            .append_message(AgentMessage::Assistant(Box::new(assistant_with_tool_call(
                "call-1", "write",
            ))))
            .await
            .unwrap();
        open_operation(&session, "run-1", "main").await;
        session
            .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
                base: record_base("ts-1", "main"),
                run_id: "run-1".to_string(),
                assistant_entry_id: assistant_id,
                tool_index: 0,
                tool_call_id: "call-1".to_string(),
                tool_name: "write".to_string(),
                effective_args: serde_json::json!({ "path": "a.txt" }),
                result_entry_id: "result-1".to_string(),
                replay: ToolReplay::Never,
            }))
            .await
            .unwrap();

        let runtime = SessionRuntime::new(session);
        let report = runtime.recover_lane("main").await.unwrap();
        assert_eq!(report.tool_frames.len(), 1, "still tracked as in flight");
        assert!(
            !report
                .findings
                .iter()
                .any(|f| matches!(f, RecoveryFinding::DanglingToolFrame { .. })),
            "a running tool is not dangling: {:?}",
            report.findings
        );
    }

    /// The intent records a compaction step writes must be a legal record log.
    ///
    /// This is the invariant that matters for the write side: the reducer
    /// deep-compares a committed entry against the `write_deferred` target it was
    /// provisioned with, and rejects a `step_attempt` whose series or result are
    /// inconsistent. Writing records the validator would reject would be worse
    /// than writing none, because a corrupt log blocks recovery outright.
    #[tokio::test]
    async fn compaction_intent_records_validate_against_the_reducer() {
        use crate::session::reducer::{validate_record_log, RecordLogSlice};
        use crate::session::types::{
            provisioned_into_entry, CompactionReason, OperationStartedRecord, ProvisionedEntry,
            ProvisionedKind, StepAttemptRecord, StepKind,
        };

        let session = session();
        let lane = "main";
        let run_id = "run-1";
        open_operation(&session, run_id, lane).await;

        // Exactly what `AgentHarness::persist_compaction_entry` builds.
        let entry = ProvisionedEntry {
            id: "compaction-1".to_string(),
            kind: ProvisionedKind::Compaction {
                summary: "summarized".to_string(),
                retained_tail: Vec::new(),
                tokens_before: 1234,
                details: None,
                usage: None,
            },
        };
        let target = crate::session::reducer::entry_provisioned_json(&provisioned_into_entry(
            entry.clone(),
            0,
            None,
            0,
        ));
        session
            .append_record(LaneRecord::WriteDeferred(WriteDeferredRecord {
                base: record_base("wd-1", lane),
                run_id: run_id.to_string(),
                target,
            }))
            .await
            .unwrap();
        session.append_entry(entry, lane).await.unwrap();
        session
            .append_record(LaneRecord::StepAttempt(StepAttemptRecord {
                base: record_base("sa-1", lane),
                run_id: run_id.to_string(),
                step: StepKind::Compaction,
                attempt: 1,
                result_entry_id: "compaction-1".to_string(),
                compaction_reason: Some(CompactionReason::Threshold),
            }))
            .await
            .unwrap();

        // The reducer accepts the log as a legal product of the record protocol.
        let slice = RecordLogSlice {
            lane: lane.to_string(),
            open_operations: vec![OperationStartedRecord {
                base: record_base(run_id, lane),
                source_leaf_id: None,
                intent: OperationIntent::Run {
                    original_prompt: Vec::new(),
                    initial_messages: Vec::new(),
                    system_prompt_override: None,
                    resume_data: None,
                },
            }],
            records: session
                .find_records(&RecordQuery {
                    lane: Some(lane.to_string()),
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap(),
            entries: session
                .find_entries(&crate::session::types::EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap(),
        };
        validate_record_log(&slice).expect("the intent records must form a legal record log");

        // And recovery sees the write as settled, not as a dangling frame.
        finish_operation(&session, run_id, lane).await;
        let runtime = SessionRuntime::new(session);
        let report = runtime.recover_lane(lane).await.unwrap();
        assert!(
            report.pending_writes.is_empty(),
            "a committed deferred write is settled: {:?}",
            report.pending_writes
        );
        assert!(report.is_clean(), "healthy session: {report:?}");
    }

    #[tokio::test]
    async fn checkpoint_detects_progress() {
        let session = session();
        let runtime = SessionRuntime::new(session);
        let before = runtime.checkpoint("main").await.unwrap();
        assert!(!before.has_open_operation);

        open_operation(&runtime.session, "run-1", "main").await;
        let after = runtime.checkpoint("main").await.unwrap();
        assert!(after.has_open_operation);
        assert!(before.is_behind(&after));
        assert!(!after.is_behind(&before));
    }

    #[tokio::test]
    async fn recover_all_lanes_sweeps_every_lane() {
        let session = session();
        open_operation(&session, "run-main", "main").await;
        let runtime = SessionRuntime::new(session);
        let all = recover_all_lanes(&runtime).await.unwrap();
        assert!(all.contains_key("main"));
        assert_eq!(all["main"].open_operations.len(), 1);
    }
}
