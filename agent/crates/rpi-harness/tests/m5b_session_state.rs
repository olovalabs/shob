//! M5b integration tests — `SessionState` invariants + `validate_record_log`
//! corruption reasons. Mirrors `packages/agent/test/harness/reducer.test.ts`
//! (corruption cases) + the `session-state` invariant slice of the plan's §5.
//!
//! These run against the public `rpi_harness::session` API only — no JSONL,
//! no harness run loop (those land in M5c/M5g).

use rpi_agent::message::AgentMessage;
use rpi_ai::types::{Api, StopReason, Usage};
use rpi_harness::session::types::*;
use rpi_harness::session::types::{EntryBase, ProvisionedEntry, ProvisionedKind, SessionMutation};
use rpi_harness::session::{
    validate_record_log, RecordLogCorruptionReason, RecordLogSlice, SessionState,
};

// ---- builders that mirror the TS `reducer.test.ts` fixtures ----

fn usage_fixture() -> Usage {
    Usage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 2,
        cost: rpi_ai::types::UsageCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    }
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
}

fn assistant_message(
    content: Vec<rpi_ai::types::Content>,
    stop_reason: StopReason,
) -> AgentMessage {
    let mut msg =
        rpi_ai::types::AssistantMessage::empty(Api::OpenaiResponses, "openai", "test-model", 1);
    msg.content = content;
    msg.stop_reason = stop_reason;
    msg.usage = usage_fixture();
    AgentMessage::Assistant(std::boxed::Box::new(msg))
}

fn tool_result_message(tool_call_id: &str, tool_name: &str) -> AgentMessage {
    AgentMessage::ToolResult(std::boxed::Box::new(rpi_ai::types::ToolResultMessage {
        role: rpi_ai::types::ToolResultRole,
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![rpi_ai::types::Content::text("result")],
        details: None,
        usage: None,
        added_tool_names: vec![],
        is_error: false,
        timestamp: 1,
    }))
}

fn msg_provisioned(id: &str, message: AgentMessage) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.to_string(),
        kind: ProvisionedKind::Message {
            message,
            terminate: None,
        },
    }
}

fn msg_entry(id: &str, message: AgentMessage, seq: u64, parent_id: Option<&str>) -> Entry {
    let base = EntryBase {
        entry_type: "message".to_string(),
        id: id.to_string(),
        seq,
        parent_id: parent_id.map(|s| s.to_string()),
        timestamp: seq as i64,
    };
    Entry::Message(MessageEntry {
        base,
        message,
        terminate: None,
    })
}

#[allow(dead_code)]
fn compaction_entry(id: &str, seq: u64) -> Entry {
    Entry::Compaction(CompactionEntry {
        base: EntryBase {
            entry_type: "compaction".to_string(),
            id: id.to_string(),
            seq,
            parent_id: None,
            timestamp: seq as i64,
        },
        summary: "summary".to_string(),
        retained_tail: vec![],
        tokens_before: 10,
        details: None,
        usage: None,
    })
}

#[allow(dead_code)]
fn branch_summary_entry(id: &str, seq: u64) -> Entry {
    Entry::BranchSummary(BranchSummaryEntry {
        base: EntryBase {
            entry_type: "branch_summary".to_string(),
            id: id.to_string(),
            seq,
            parent_id: Some("target".to_string()),
            timestamp: seq as i64,
        },
        from_id: "source".to_string(),
        summary: "summary".to_string(),
        details: None,
        usage: None,
    })
}

fn rec_base(id: &str, seq: u64, lane: &str) -> RecordBase {
    RecordBase {
        id: id.to_string(),
        seq,
        lane: lane.to_string(),
        timestamp: seq as i64,
    }
}

fn run_started(seq: u64, id: &str) -> OperationStartedRecord {
    OperationStartedRecord {
        base: rec_base(id, seq, "main"),
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: vec![],
            initial_messages: vec![],
            system_prompt_override: None,
            resume_data: None,
        },
    }
}

fn run_started_with_initials(
    seq: u64,
    id: &str,
    initials: Vec<serde_json::Value>,
) -> OperationStartedRecord {
    OperationStartedRecord {
        base: rec_base(id, seq, "main"),
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: vec![],
            initial_messages: initials,
            system_prompt_override: None,
            resume_data: None,
        },
    }
}

fn abort_requested(seq: u64, id: &str, run_id: &str) -> LaneRecord {
    LaneRecord::AbortRequested(AbortRequestedRecord {
        base: rec_base(id, seq, "main"),
        run_id: run_id.to_string(),
    })
}

fn operation_finished(seq: u64, id: &str, run_id: &str) -> LaneRecord {
    LaneRecord::OperationFinished(OperationFinishedRecord {
        base: rec_base(id, seq, "main"),
        run_id: run_id.to_string(),
        outcome: OperationOutcome::Completed,
        error: None,
        result: None,
    })
}

fn attempt(
    seq: u64,
    id: &str,
    run_id: &str,
    step: StepKind,
    attempt_no: u32,
    result_id: &str,
    compaction_reason: Option<CompactionReason>,
) -> LaneRecord {
    LaneRecord::StepAttempt(StepAttemptRecord {
        base: rec_base(id, seq, "main"),
        run_id: run_id.to_string(),
        step,
        attempt: attempt_no,
        result_entry_id: result_id.to_string(),
        compaction_reason,
    })
}

fn recovery_slice(records: Vec<LaneRecord>, entries: Vec<Entry>) -> RecordLogSlice {
    // Open operations = operation_started records with no matching operation_finished,
    // newest first.
    let finished: std::collections::HashSet<String> = records
        .iter()
        .filter_map(|r| match r {
            LaneRecord::OperationFinished(f) => Some(f.run_id.clone()),
            _ => None,
        })
        .collect();
    let mut open: Vec<OperationStartedRecord> = records
        .iter()
        .filter_map(|r| match r {
            LaneRecord::OperationStarted(s) if !finished.contains(&s.base.id) => Some(s.clone()),
            _ => None,
        })
        .collect();
    open.sort_by(|a, b| b.base.seq.cmp(&a.base.seq));
    RecordLogSlice {
        lane: "main".to_string(),
        open_operations: open,
        records,
        entries,
    }
}

fn expect_corruption(slice: &RecordLogSlice, reason: RecordLogCorruptionReason) {
    match validate_record_log(slice) {
        Ok(()) => panic!("expected corruption {reason:?}, but slice validated"),
        Err(e) => assert_eq!(e.reason, reason, "wrong corruption reason: {}", e.message),
    }
}

// ===== SessionState invariants =====

#[test]
fn state_rejects_non_consecutive_seq() {
    let mut state = SessionState::new();
    // First mutation must be seq=1.
    let err = state
        .apply_mutation(SessionMutation::Lane {
            seq: 5,
            lane: "main".to_string(),
            leaf_id: None,
        })
        .unwrap_err();
    assert!(err.to_string().contains("non-consecutive seq"));
}

#[test]
fn clears_retire_frames_from_memory_and_keep_the_log_in_step() {
    let frame = |seq: u64, stream_index, op| {
        LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: rec_base(&format!("frame-{seq}"), seq, "main"),
            run_id: "run".to_string(),
            stream_index,
            op,
            frame: None,
        })
    };
    let mut state = SessionState::new();
    for record in [
        frame(1, 0, AssistantFrameOp::Append),
        abort_requested(2, "abort", "run"),
        frame(3, 1, AssistantFrameOp::Append),
        frame(4, 0, AssistantFrameOp::ClearStream),
    ] {
        state
            .apply_mutation(SessionMutation::Record { record })
            .unwrap();
    }
    let log_seqs = |state: &SessionState| -> Vec<u64> {
        let log = state.get_log(&LogOptions::default()).unwrap();
        log.iter().map(|item| item.seq()).collect()
    };
    assert_eq!(log_seqs(&state), [2, 3, 4]);

    state
        .apply_mutation(SessionMutation::Record {
            record: frame(5, 0, AssistantFrameOp::ClearRun),
        })
        .unwrap();
    assert_eq!(log_seqs(&state), [2, 4, 5]);
}

#[test]
fn retired_frames_keep_their_ids_reserved() {
    let frame = |id: &str, seq: u64, op| {
        LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: rec_base(id, seq, "main"),
            run_id: "run".to_string(),
            stream_index: 0,
            op,
            frame: None,
        })
    };
    let mut state = SessionState::new();
    for record in [
        frame("frame", 1, AssistantFrameOp::Append),
        frame("clear", 2, AssistantFrameOp::ClearRun),
    ] {
        state
            .apply_mutation(SessionMutation::Record { record })
            .unwrap();
    }

    assert!(state.validate_unused_id("frame").is_err());
    let err = state
        .apply_mutation(SessionMutation::Record {
            record: frame("frame", 3, AssistantFrameOp::Append),
        })
        .unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn state_rejects_write_once_id_reuse() {
    let mut state = SessionState::new();
    let user = user_message("hi");
    // seq=1: lane creation (no — we go straight to an entry on main).
    state
        .apply_mutation(SessionMutation::Entry {
            seq: 1,
            timestamp: 1,
            lane: Some("main".to_string()),
            entry: msg_entry("e1", user.clone(), 1, None),
        })
        .unwrap();
    let err = state
        .apply_mutation(SessionMutation::Entry {
            seq: 2,
            timestamp: 2,
            lane: Some("main".to_string()),
            // Reuses id "e1" but seq=2, parent=e1 — id collision is the error.
            entry: msg_entry("e1", user, 2, Some("e1")),
        })
        .unwrap_err();
    assert!(err.to_string().contains("id already exists"));
}

#[test]
fn state_lane_leaf_chaining_enforced() {
    let mut state = SessionState::new();
    // Append e1 on main (parent None since main leaf is None).
    state
        .apply_mutation(SessionMutation::Entry {
            seq: 1,
            timestamp: 1,
            lane: Some("main".to_string()),
            entry: msg_entry("e1", user_message("a"), 1, None),
        })
        .unwrap();
    // Now main leaf is e1. An entry claiming parent_id=None does NOT chain.
    let err = state
        .apply_mutation(SessionMutation::Entry {
            seq: 2,
            timestamp: 2,
            lane: Some("main".to_string()),
            entry: msg_entry("e2", user_message("b"), 2, None),
        })
        .unwrap_err();
    assert!(err.to_string().contains("does not chain to the lane leaf"));
}

#[test]
fn state_fork_omits_lane_so_chaining_is_skipped() {
    // The fork path passes `lane: None`, so lane-leaf CHAINING is not checked
    // (invariant §7). Demonstrate by first anchoring `main`'s leaf to `e1`
    // via a normal lane-bearing append, then appending `e2` via the fork path
    // with `parent_id: None` — which would be rejected if chaining were
    // checked (main's leaf is `e1`, not `None`), but is accepted because
    // `lane: None` skips the check.
    let mut state = SessionState::new();
    state
        .apply_mutation(SessionMutation::Entry {
            seq: 1,
            timestamp: 1,
            lane: Some("main".to_string()),
            entry: msg_entry("e1", user_message("a"), 1, None),
        })
        .unwrap();

    // Fork path: lane=None, parent_id=None does NOT equal main's leaf (e1).
    // Accepted because chaining is skipped on the fork path.
    state
        .apply_mutation(SessionMutation::Entry {
            seq: 2,
            timestamp: 2,
            lane: None,
            entry: msg_entry("e2", user_message("b"), 2, None),
        })
        .unwrap();
    assert_eq!(state.next_sequence(), 3);

    // Contrast: the same `parent_id: None` entry on a `lane: Some("main")` path
    // IS rejected — main's leaf is `e1`, so the entry does not chain.
    let err = state
        .apply_mutation(SessionMutation::Entry {
            seq: 3,
            timestamp: 3,
            lane: Some("main".to_string()),
            entry: msg_entry("e3", user_message("c"), 3, None),
        })
        .unwrap_err();
    assert!(err.to_string().contains("does not chain to the lane leaf"));
}

#[test]
fn state_open_operation_tracking_records_and_removes() {
    use rpi_harness::session::types::OperationStartedRecord;
    let mut state = SessionState::new();
    let started = LaneRecord::OperationStarted(OperationStartedRecord {
        base: rec_base("op-1", 1, "main"),
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: vec![],
            initial_messages: vec![],
            system_prompt_override: None,
            resume_data: None,
        },
    });
    state
        .apply_mutation(SessionMutation::Record { record: started })
        .unwrap();
    let open = state.find_open_operations("main", Some(2)).unwrap();
    assert_eq!(open.len(), 1);

    state
        .apply_mutation(SessionMutation::Record {
            record: operation_finished(2, "finish-1", "op-1"),
        })
        .unwrap();
    let open = state.find_open_operations("main", Some(2)).unwrap();
    assert!(open.is_empty(), "operation_finished removes the open op");
}

#[test]
fn state_facts_latest_wins() {
    let mut state = SessionState::new();
    state
        .apply_mutation(SessionMutation::FactName {
            seq: 1,
            name: Some("first".to_string()),
        })
        .unwrap();
    state
        .apply_mutation(SessionMutation::FactName {
            seq: 2,
            name: Some("second".to_string()),
        })
        .unwrap();
    assert_eq!(state.get_name(), Some("second"));
}

// ===== reducer corruption reasons =====

#[test]
fn reducer_multiple_open_operations() {
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            LaneRecord::OperationStarted(run_started(2, "run-2")),
        ],
        vec![],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::MultipleOpenOperations);
}

#[test]
fn reducer_unknown_operation() {
    let slice = recovery_slice(vec![abort_requested(1, "abort-1", "missing")], vec![]);
    expect_corruption(&slice, RecordLogCorruptionReason::UnknownOperation);
}

#[test]
fn reducer_record_after_finish() {
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            operation_finished(2, "finish-1", "run-1"),
            abort_requested(3, "abort-1", "run-1"),
        ],
        vec![],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::RecordAfterFinish);
}

fn legacy_closed_queue_records(kind: QueueKind) -> Vec<LaneRecord> {
    vec![
        LaneRecord::OperationStarted(run_started(1, "run-1")),
        LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
            base: rec_base("enqueue-1", 2, "main"),
            queue: kind,
            run_id: Some("run-1".into()),
            target: provisioned_json(msg_provisioned("queue-1", user_message("queued"))),
        }),
        operation_finished(3, "finish-1", "run-1"),
    ]
}

fn legacy_queue_cancel(run_id: Option<&str>, entry_id: &str) -> LaneRecord {
    LaneRecord::QueueCancelled(QueueCancelledRecord {
        base: rec_base("cancel-1", 6, "main"),
        run_id: run_id.map(str::to_string),
        entry_id: entry_id.into(),
    })
}

#[test]
fn reducer_legacy_queue_cancellation_after_finish_is_valid() {
    for kind in [QueueKind::Steer, QueueKind::FollowUp, QueueKind::NextRun] {
        let mut records = legacy_closed_queue_records(kind);
        records.push(legacy_queue_cancel(Some("run-1"), "queue-1"));
        validate_record_log(&recovery_slice(records, vec![])).unwrap();
    }
}

#[test]
fn reducer_late_legacy_cancellation_requires_a_matching_unconsumed_enqueue() {
    for (run_id, entry_id, consumed, reason) in [
        (
            None,
            "queue-1",
            false,
            RecordLogCorruptionReason::InvalidQueueCancellation,
        ),
        (
            Some("run-2"),
            "queue-1",
            false,
            RecordLogCorruptionReason::InvalidQueueCancellation,
        ),
        (
            Some("missing-run"),
            "queue-1",
            false,
            RecordLogCorruptionReason::UnknownOperation,
        ),
        (
            Some("run-1"),
            "missing-entry",
            false,
            RecordLogCorruptionReason::InvalidQueueCancellation,
        ),
        (
            Some("run-1"),
            "queue-1",
            true,
            RecordLogCorruptionReason::InvalidQueueCancellation,
        ),
    ] {
        let mut records = legacy_closed_queue_records(QueueKind::FollowUp);
        records.push(LaneRecord::OperationStarted(run_started(4, "run-2")));
        records.push(operation_finished(5, "finish-2", "run-2"));
        records.push(legacy_queue_cancel(run_id, entry_id));
        let entries = if consumed {
            vec![msg_entry("queue-1", user_message("queued"), 7, None)]
        } else {
            vec![]
        };
        expect_corruption(&recovery_slice(records, entries), reason);
    }
}

#[test]
fn reducer_finish_barrier_still_rejects_new_enqueues_and_usage() {
    let invalid = [
        LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
            base: rec_base("late-enqueue", 4, "main"),
            queue: QueueKind::FollowUp,
            run_id: Some("run-1".into()),
            target: provisioned_json(msg_provisioned("queue-2", user_message("late"))),
        }),
        LaneRecord::Usage(UsageRecord {
            base: rec_base("late-usage", 4, "main"),
            usage: usage_fixture(),
            cause: UsageCause::Assistant,
            run_id: Some("run-1".into()),
            entry_id: None,
            attempt: None,
            stop_reason: None,
            tool_call_id: None,
            details: None,
        }),
    ];
    for record in invalid {
        let mut records = legacy_closed_queue_records(QueueKind::Steer);
        records.push(record);
        expect_corruption(
            &recovery_slice(records, vec![]),
            RecordLogCorruptionReason::RecordAfterFinish,
        );
    }
}

#[test]
fn reducer_non_consecutive_attempt() {
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            attempt(
                2,
                "a-1",
                "run-1",
                StepKind::Assistant,
                1,
                "assistant-1",
                None,
            ),
            attempt(
                3,
                "a-2",
                "run-1",
                StepKind::Assistant,
                3,
                "assistant-2",
                None,
            ),
        ],
        vec![],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::NonConsecutiveAttempt);
}

#[test]
fn reducer_invalid_compaction_reason_omitted() {
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            attempt(2, "a-1", "run-1", StepKind::Compaction, 1, "c-1", None),
        ],
        vec![],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::InvalidCompactionReason);
}

#[test]
fn reducer_invalid_compaction_reason_on_non_compaction() {
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            attempt(
                2,
                "a-1",
                "run-1",
                StepKind::Assistant,
                1,
                "a-1",
                Some(CompactionReason::Manual),
            ),
        ],
        vec![],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::InvalidCompactionReason);
}

#[test]
fn reducer_queue_after_abort() {
    // run-1 started, aborted, then a steer enqueue arrives.
    let target = provisioned_json(msg_provisioned("queue-1", user_message("queued")));
    let enqueue = LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
        base: rec_base("q-3", 3, "main"),
        queue: QueueKind::Steer,
        run_id: Some("run-1".to_string()),
        target,
    });
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            abort_requested(2, "abort-1", "run-1"),
            enqueue,
        ],
        vec![],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::QueueAfterAbort);
}

#[test]
fn reducer_invalid_queue_cancellation_no_enqueue() {
    let cancel = LaneRecord::QueueCancelled(QueueCancelledRecord {
        base: rec_base("c-2", 2, "main"),
        run_id: Some("run-1".to_string()),
        entry_id: "queue-1".to_string(),
    });
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            cancel,
        ],
        vec![],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::InvalidQueueCancellation);
}

#[test]
fn reducer_inconsistent_step_result_id() {
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            attempt(
                2,
                "a-1",
                "run-1",
                StepKind::Compaction,
                1,
                "c-1",
                Some(CompactionReason::Threshold),
            ),
            attempt(
                3,
                "a-2",
                "run-1",
                StepKind::Compaction,
                2,
                "c-2",
                Some(CompactionReason::Threshold),
            ),
        ],
        vec![],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::InconsistentStep);
}

#[test]
fn reducer_tool_call_mismatch() {
    let assistant_tools = msg_entry(
        "assistant-tools",
        assistant_message(
            vec![rpi_ai::types::Content::tool_call(
                "call-1",
                "tool-1",
                serde_json::json!({}),
            )],
            StopReason::ToolUse,
        ),
        1,
        None,
    );
    let tool_start = LaneRecord::ToolStarted(ToolStartedRecord {
        base: rec_base("ts-1", 4, "main"),
        run_id: "run-1".to_string(),
        assistant_entry_id: "assistant-tools".to_string(),
        tool_index: 0,
        tool_call_id: "different-call".to_string(),
        tool_name: "tool-1".to_string(),
        effective_args: serde_json::json!({}),
        result_entry_id: "tool-result-1".to_string(),
        replay: ToolReplay::Never,
    });
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            tool_start,
        ],
        vec![assistant_tools],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::ToolCallMismatch);
}

#[test]
fn reducer_duplicate_tool_invocation() {
    let assistant_tools = msg_entry(
        "assistant-tools",
        assistant_message(
            vec![rpi_ai::types::Content::tool_call(
                "call-1",
                "tool-1",
                serde_json::json!({}),
            )],
            StopReason::ToolUse,
        ),
        1,
        None,
    );
    let ts1 = ToolStartedRecord {
        base: rec_base("ts-1", 4, "main"),
        run_id: "run-1".to_string(),
        assistant_entry_id: "assistant-tools".to_string(),
        tool_index: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "tool-1".to_string(),
        effective_args: serde_json::json!({}),
        result_entry_id: "tool-result-1".to_string(),
        replay: ToolReplay::Never,
    };
    let mut ts2 = ts1.clone();
    ts2.base.id = "ts-duplicate".to_string();
    ts2.result_entry_id = "tool-result-2".to_string();
    let slice = recovery_slice(
        vec![
            LaneRecord::OperationStarted(run_started(1, "run-1")),
            LaneRecord::ToolStarted(ts1),
            LaneRecord::ToolStarted(ts2),
        ],
        vec![assistant_tools],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::DuplicateToolInvocation);
}

#[test]
fn reducer_provisioned_entry_mismatch() {
    let target_json = provisioned_json(msg_provisioned("prompt-1", user_message("expected")));
    let started = run_started_with_initials(1, "run-1", vec![target_json]);
    // Persisted entry has the SAME id but DIFFERENT content.
    let persisted = msg_entry("prompt-1", user_message("different"), 2, None);
    let slice = recovery_slice(vec![LaneRecord::OperationStarted(started)], vec![persisted]);
    expect_corruption(&slice, RecordLogCorruptionReason::ProvisionedEntryMismatch);
}

#[test]
fn reducer_invalid_deferred_handle() {
    // An assistant message with stop_reason=Deferred but no deferred handle.
    let mut msg = assistant_message(vec![], StopReason::Deferred);
    if let AgentMessage::Assistant(ref mut a) = msg {
        a.deferred = None;
    }
    let persisted = msg_entry("assistant-deferred", msg, 2, None);
    let slice = recovery_slice(
        vec![LaneRecord::OperationStarted(run_started(1, "run-1"))],
        vec![persisted],
    );
    expect_corruption(&slice, RecordLogCorruptionReason::InvalidDeferredHandle);
}

#[test]
fn reducer_valid_one_tool_run_prefixes() {
    // A prefix of a legal one-tool run validates at every step.
    let prompt = msg_provisioned("prompt-1", user_message("fix the bug"));
    let prompt_json = provisioned_json(prompt.clone());
    let assistant_tools = msg_entry(
        "assistant-tools",
        assistant_message(
            vec![rpi_ai::types::Content::tool_call(
                "call-1",
                "tool-1",
                serde_json::json!({}),
            )],
            StopReason::ToolUse,
        ),
        4,
        Some("prompt-1"),
    );
    let tool_result = msg_entry(
        "tool-result-1",
        tool_result_message("call-1", "tool-1"),
        6,
        Some("assistant-tools"),
    );
    let assistant_final = msg_entry(
        "assistant-final",
        assistant_message(vec![rpi_ai::types::Content::text("done")], StopReason::Stop),
        8,
        Some("tool-result-1"),
    );

    let actions: Vec<VariantAction> = vec![
        VariantAction::Record(LaneRecord::OperationStarted(run_started_with_initials(
            1,
            "run-1",
            vec![prompt_json],
        ))),
        VariantAction::Entry(msg_entry("prompt-1", user_message("fix the bug"), 2, None)),
        VariantAction::Record(attempt(
            3,
            "a-1",
            "run-1",
            StepKind::Assistant,
            1,
            "assistant-tools",
            None,
        )),
        VariantAction::Entry(assistant_tools),
        VariantAction::Record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: rec_base("ts-5", 5, "main"),
            run_id: "run-1".to_string(),
            assistant_entry_id: "assistant-tools".to_string(),
            tool_index: 0,
            tool_call_id: "call-1".to_string(),
            tool_name: "tool-1".to_string(),
            effective_args: serde_json::json!({}),
            result_entry_id: "tool-result-1".to_string(),
            replay: ToolReplay::Never,
        })),
        VariantAction::Entry(tool_result),
        VariantAction::Record(attempt(
            7,
            "a-2",
            "run-1",
            StepKind::Assistant,
            1,
            "assistant-final",
            None,
        )),
        VariantAction::Entry(assistant_final),
        VariantAction::Record(operation_finished(9, "finish-1", "run-1")),
    ];
    for prefix in valid_prefixes(&actions) {
        validate_record_log(&prefix).expect("prefix should validate");
    }
}

#[test]
fn reducer_does_not_mutate_inputs() {
    let target = msg_provisioned("prompt-1", user_message("hello"));
    let target_json = provisioned_json(target.clone());
    let started = run_started_with_initials(1, "run-1", vec![target_json]);
    let entry = msg_entry("prompt-1", user_message("hello"), 2, None);
    let slice = RecordLogSlice {
        lane: "main".to_string(),
        open_operations: vec![started.clone()],
        records: vec![LaneRecord::OperationStarted(started)],
        entries: vec![entry.clone()],
    };
    validate_record_log(&slice).expect("valid");
    assert_eq!(slice.records.len(), 1);
    assert_eq!(slice.entries.len(), 1);
    assert_eq!(slice.entries[0].id(), entry.id());
}

// ---- prefix helpers ----

enum VariantAction {
    Record(LaneRecord),
    Entry(Entry),
}

fn valid_prefixes(actions: &[VariantAction]) -> Vec<RecordLogSlice> {
    let mut out = Vec::new();
    for i in 0..actions.len() {
        let prefix = &actions[..=i];
        let records: Vec<LaneRecord> = prefix
            .iter()
            .filter_map(|a| match a {
                VariantAction::Record(r) => Some(r.clone()),
                _ => None,
            })
            .collect();
        let entries: Vec<Entry> = prefix
            .iter()
            .filter_map(|a| match a {
                VariantAction::Entry(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        out.push(recovery_slice(records, entries));
    }
    out
}

fn provisioned_json(p: ProvisionedEntry) -> serde_json::Value {
    // Use Entry::to_flat_json through a temporary stamp, then strip storage fields.
    let entry = rpi_harness::session::types::provisioned_into_entry(p, 0, None, 0);
    let mut value = entry.to_flat_json();
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("seq");
        map.remove("parentId");
        map.remove("timestamp");
    }
    value
}
