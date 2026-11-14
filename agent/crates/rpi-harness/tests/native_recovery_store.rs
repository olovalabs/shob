//! Native facts, real reducer oracle, isolated versioned store only.
#[path = "support/native_recovery_store.rs"]
mod native;
use native::{Fault, Identity, Meta, Result, Store, TAIL};
use rpi_harness::session::{types::*, SessionState};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn identity() -> Identity {
    Identity {
        session: "fixture-session".into(),
        generation: "native-001".into(),
    }
}
fn record(mut value: Value) -> SessionMutation {
    value["id"] = value.get("id").cloned().unwrap_or(json!("record"));
    value["lane"] = value.get("lane").cloned().unwrap_or(json!("main"));
    value["seq"] = json!(0);
    value["timestamp"] = json!(42);
    SessionMutation::Record {
        record: serde_json::from_value(value).unwrap(),
    }
}
fn entry(
    id: &str,
    kind: &str,
    lane: Option<&str>,
    parent: Option<&str>,
    mut fields: Value,
) -> SessionMutation {
    fields["type"] = json!(kind);
    fields["id"] = json!(id);
    fields["seq"] = json!(0);
    fields["timestamp"] = json!(42);
    fields["parentId"] = json!(parent);
    SessionMutation::Entry {
        seq: 0,
        timestamp: 42,
        lane: lane.map(str::to_owned),
        entry: serde_json::from_value(fields).unwrap(),
    }
}
fn user(id: &str, parent: Option<&str>, text: &str) -> SessionMutation {
    entry(
        id,
        "message",
        Some("main"),
        parent,
        json!({"message":rpi_agent::AgentMessage::from(rpi_ai::types::UserMessage::new(text,42))}),
    )
}
fn stamp(mut mutation: SessionMutation, seq: u64) -> SessionMutation {
    match &mut mutation {
        SessionMutation::Entry { seq: s, entry, .. } => {
            *s = seq;
            entry.base_mut().seq = seq;
        }
        SessionMutation::Record { record } => {
            let mut value = serde_json::to_value(&*record).unwrap();
            value["seq"] = json!(seq);
            *record = serde_json::from_value(value).unwrap();
        }
        SessionMutation::Lane { seq: s, .. }
        | SessionMutation::FactName { seq: s, .. }
        | SessionMutation::FactLabel { seq: s, .. } => *s = seq,
    }
    mutation
}
fn enqueue(id: &str, lane: &str, target: Value, queue: &str) -> SessionMutation {
    record(json!({"type":"queue_enqueued","id":id,"lane":lane,"queue":queue,"target":target}))
}
fn start(id: &str) -> SessionMutation {
    record(
        json!({"type":"operation_started","id":id,"sourceLeafId":null,"intent":{"kind":"run","originalPrompt":[],"initialMessages":[]}}),
    )
}
fn cancel(id: &str, lane: &str, target: &str) -> SessionMutation {
    record(json!({"type":"queue_cancelled","id":id,"lane":lane,"entryId":target}))
}
struct Fixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Store,
    oracle: SessionState,
    seq: u64,
    labels: Vec<String>,
}
impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("native.rpidb");
        let store = Store::create(&path, identity())?;
        Ok(Self {
            _directory: directory,
            path,
            store,
            oracle: SessionState::new(),
            seq: 0,
            labels: Vec::new(),
        })
    }
    fn append(&mut self, mutation: SessionMutation) -> Result<SessionMutation> {
        let mutation = stamp(mutation, self.seq + 1);
        self.store.append(&mutation, Some(self.seq), Fault::None)?;
        self.oracle.apply_mutation(mutation.clone())?;
        self.seq += 1;
        if let SessionMutation::FactLabel { target_id, .. } = &mutation {
            self.labels.push(target_id.clone());
        }
        assert_eq!(self.store.mutation(self.seq)?, mutation);
        self.compare()?;
        Ok(mutation)
    }
    fn compare(&mut self) -> Result<()> {
        self.store.forbid_bodies(true);
        let (state, reads) = self.store.restore()?;
        assert!(reads.tail_rows < TAIL as usize);
        assert_eq!(state.seq, self.seq);
        assert_eq!(
            state.heads,
            self.oracle
                .get_lanes()
                .into_iter()
                .map(|p| (p.lane, p.leaf_id))
                .collect::<BTreeMap<_, _>>()
        );
        assert_eq!(state.stats, *self.oracle.get_stats());
        assert!(
            self.store.mutation(1).is_err(),
            "SQL guard must deny actual body reads"
        );
        self.store.forbid_bodies(false);
        for lane in state.heads.keys() {
            assert_eq!(
                self.store.open_operations(lane)?,
                self.oracle.find_open_operations(lane, None)?
            );
            assert_eq!(
                self.store.pending(lane)?,
                self.oracle.find_records(&RecordQuery {
                    lane: Some(lane.clone()),
                    record_type: Some("queue_enqueued"),
                    pending_queue_only: true,
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })?
            );
        }
        assert_eq!(self.store.name()?.as_deref(), self.oracle.get_name());
        for target in &self.labels {
            assert_eq!(
                self.store.label(target)?.as_deref(),
                self.oracle.get_label(target)
            );
        }
        Ok(())
    }
    fn reopen(&mut self) -> Result<()> {
        self.store = Store::open(&self.path, identity())?;
        self.compare()
    }
}

#[test]
fn real_queue_semantics_include_pre_cancel_duplicate_targets_and_all_entry_commits() -> Result<()> {
    let mut f = Fixture::new()?;
    f.append(SessionMutation::Lane {
        seq: 0,
        lane: "side".into(),
        leaf_id: None,
    })?;
    f.append(cancel("pre-cancel", "main", "never-pending"))?;
    f.append(enqueue(
        "pre-cancelled-enqueue",
        "main",
        json!({"id":"never-pending"}),
        "steer",
    ))?;
    f.append(enqueue(
        "dup-a",
        "main",
        json!({"id":"future","body":"first"}),
        "followUp",
    ))?;
    f.append(enqueue(
        "dup-b",
        "main",
        json!({"id":"future","body":"second"}),
        "nextRun",
    ))?;
    f.append(enqueue(
        "side-pending",
        "side",
        json!({"id":"future"}),
        "context",
    ))?;
    assert_eq!(f.store.pending("main")?.len(), 2);
    f.append(cancel("cancel-main", "main", "future"))?;
    assert_eq!(f.store.pending("side")?.len(), 1);
    f.append(entry(
        "future",
        "custom",
        None,
        None,
        json!({"customType":"commit","data":{"opaque":"not a message"}}),
    ))?;
    assert!(f.store.pending("side")?.is_empty());
    f.append(enqueue(
        "committed-enqueue",
        "main",
        json!({"id":"future"}),
        "steer",
    ))?;
    f.append(enqueue(
        "malformed-target",
        "main",
        json!({"noId":true}),
        "steer",
    ))?;
    f.append(enqueue(
        "nonstring-target",
        "main",
        json!({"id":7}),
        "steer",
    ))?;
    f.append(enqueue(
        "valid-target",
        "main",
        json!({"id":"pending","large":"x".repeat(8192)}),
        "context",
    ))?;
    f.append(SessionMutation::Lane {
        seq: 0,
        lane: "side".into(),
        leaf_id: Some("future".into()),
    })?;
    while f.seq % TAIL != 0 {
        f.append(SessionMutation::FactName {
            seq: 0,
            name: Some("checkpoint padding".into()),
        })?;
    }
    f.reopen()?;
    assert_eq!(f.store.pending("main")?.len(), 1);
    Ok(())
}

#[test]
fn all_entry_variants_lane_upserts_fork_and_lazy_facts_match_real_state() -> Result<()> {
    let mut f = Fixture::new()?;
    f.append(user("user", None, "first message"))?;
    let entries = [
        (
            "model",
            "model_change",
            json!({"provider":"fixture","modelId":"model"}),
        ),
        (
            "thinking",
            "thinking_level_change",
            json!({"thinkingLevel":"high"}),
        ),
        (
            "tools",
            "active_tools_change",
            json!({"activeToolNames":["fixture"]}),
        ),
        (
            "compact",
            "compaction",
            json!({"summary":"summary","retainedTail":[],"tokensBefore":123}),
        ),
        (
            "summary",
            "branch_summary",
            json!({"fromId":"user","summary":"branch summary"}),
        ),
        (
            "custom",
            "custom",
            json!({"customType":"fixture","data":{"body":"x".repeat(32000)}}),
        ),
    ];
    let mut parent = "user";
    for (id, kind, fields) in entries {
        f.append(entry(id, kind, Some("main"), Some(parent), fields))?;
        parent = id;
    }
    f.append(entry(
        "fork",
        "custom",
        None,
        Some("user"),
        json!({"customType":"fork"}),
    ))?;
    f.append(SessionMutation::Lane {
        seq: 0,
        lane: "side".into(),
        leaf_id: Some("fork".into()),
    })?;
    f.append(SessionMutation::Lane {
        seq: 0,
        lane: "side".into(),
        leaf_id: Some("custom".into()),
    })?;
    for i in 0..20 {
        f.append(SessionMutation::FactName {
            seq: 0,
            name: Some(format!("name-{i}")),
        })?;
        f.append(SessionMutation::FactLabel {
            seq: 0,
            target_id: "user".into(),
            label: Some(format!("label-{i}")),
        })?;
    }
    f.append(SessionMutation::FactName { seq: 0, name: None })?;
    f.append(SessionMutation::FactLabel {
        seq: 0,
        target_id: "user".into(),
        label: None,
    })?;
    f.reopen()?;
    assert_eq!(f.store.restore()?.0.stats.message_count, 1);
    Ok(())
}

#[test]
fn all_record_variants_and_real_operation_validator_are_preserved() -> Result<()> {
    let mut f = Fixture::new()?;
    f.append(start("run"))?;
    let records = vec![
        json!({"type":"step_attempt","id":"step","runId":"run","step":"assistant","attempt":1,"resultEntryId":"assistant"}),
        json!({"type":"tool_started","id":"tool","runId":"run","assistantEntryId":"assistant","toolIndex":0,"toolCallId":"call","toolName":"fixture","effectiveArgs":{"huge":"x".repeat(64000)},"resultEntryId":"tool-result","replay":"never"}),
        json!({"type":"write_deferred","id":"deferred","runId":"run","target":{"id":"deferred-entry","opaque":"x".repeat(12000)}}),
        json!({"type":"assistant_frame","id":"frame","runId":"run","streamIndex":0,"op":"clear_run"}),
        json!({"type":"retry_pending","id":"retry","runId":"run","attempt":2,"resultEntryId":"retry-result"}),
        json!({"type":"usage","id":"usage","cause":"assistant","runId":"run","usage":{"input":10,"output":20,"cacheRead":3,"cacheWrite":2,"totalTokens":35,"cost":{"input":0.125,"output":0.25,"cacheRead":0.0,"cacheWrite":0.0,"total":0.375}}}),
        json!({"type":"abort_requested","id":"abort","runId":"run"}),
        json!({"type":"operation_finished","id":"finish-unknown","runId":"unknown","outcome":"completed"}),
    ];
    for value in records {
        f.append(record(value))?;
    }
    f.append(enqueue("queue", "main", json!({"id":"queued"}), "followUp"))?;
    f.append(cancel("cancel", "main", "queued"))?;
    f.append(start("second-run"))?;
    assert_eq!(f.store.open_operations("main")?.len(), 2);
    let slice = rpi_harness::session::RecordLogSlice {
        lane: "main".into(),
        open_operations: f.store.open_operations("main")?,
        ..Default::default()
    };
    let invalid = rpi_harness::session::validate_record_log(&slice).unwrap_err();
    assert_eq!(
        invalid.reason,
        rpi_harness::session::RecordLogCorruptionReason::MultipleOpenOperations
    );
    f.append(record(json!({"type":"operation_finished","id":"finish","runId":"run","outcome":"failed","error":{"code":"fixture","message":"opaque error"}})))?;
    while f.seq % TAIL != 0 {
        f.append(SessionMutation::FactName { seq: 0, name: None })?;
    }
    f.reopen()?;
    assert_eq!(f.store.open_operations("main")?[0].base.id, "second-run");
    Ok(())
}

fn assert_error<T>(result: Result<T>, expected: &str) {
    let error = result.err().expect("must reject").to_string();
    assert!(
        error.contains(expected),
        "expected {expected:?}, got {error:?}"
    );
}
fn edit_database(path: &std::path::Path, sql: &str) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

#[test]
fn ack_reconciliation_checks_full_native_request_not_just_existing_id() -> Result<()> {
    let mut f = Fixture::new()?;
    let request = stamp(user("uncertain", None, "original body"), 1);
    assert_error(
        f.store.append(&request, Some(0), Fault::LostAck),
        "outcome uncertain",
    );
    f.store = Store::open(&f.path, identity())?;
    assert!(f.store.reconcile(&request)?);
    assert_eq!(f.store.mutation_id("uncertain")?, Some(request.clone()));
    assert_error(
        f.store
            .reconcile(&stamp(user("uncertain", None, "different body"), 1)),
        "reconcile content mismatch",
    );
    assert!(!f
        .store
        .reconcile(&stamp(user("not-written", None, "body"), 2))?);
    let next = stamp(user("next", Some("uncertain"), "after failed append"), 2);
    assert_error(
        f.store.append(&next, Some(1), Fault::BeforeCommit),
        "precommit failure",
    );
    assert!(!f.store.reconcile(&next)?);
    assert_eq!(f.store.restore()?.0.seq, 1);
    f.store.append(&next, Some(1), Fault::None)?;
    let fact = SessionMutation::FactName {
        seq: 3,
        name: Some("name".into()),
    };
    assert_error(
        f.store.append(&fact, Some(2), Fault::LostAck),
        "outcome uncertain",
    );
    assert!(f.store.reconcile(&fact)?);
    assert_error(
        f.store
            .reconcile(&SessionMutation::FactName { seq: 3, name: None }),
        "reconcile content mismatch",
    );
    Ok(())
}

#[test]
fn native_invariants_and_stale_handles_fail_without_consuming_sequence() -> Result<()> {
    let mut f = Fixture::new()?;
    f.append(user("first", None, "body"))?;
    let mut other = Store::open(&f.path, identity())?;
    f.append(user("second", Some("first"), "body"))?;
    assert_error(
        other.append(
            &stamp(user("stale", Some("first"), "body"), 2),
            Some(1),
            Fault::None,
        ),
        "stale native revision",
    );
    let bad = vec![
        stamp(user("bad-parent", Some("first"), "body"), 3),
        stamp(
            entry(
                "orphan",
                "custom",
                None,
                Some("unknown"),
                json!({"customType":"test"}),
            ),
            3,
        ),
        stamp(
            record(json!({"type":"abort_requested","id":"bad-lane","lane":"absent","runId":"run"})),
            3,
        ),
        SessionMutation::Lane {
            seq: 3,
            lane: "side".into(),
            leaf_id: Some("unknown".into()),
        },
        SessionMutation::FactLabel {
            seq: 3,
            target_id: "unknown".into(),
            label: None,
        },
        stamp(
            entry(
                "first",
                "custom",
                None,
                None,
                json!({"customType":"duplicate"}),
            ),
            3,
        ),
    ];
    for mutation in bad {
        assert!(f.oracle.clone().apply_mutation(mutation.clone()).is_err());
        assert!(f.store.append(&mutation, None, Fault::None).is_err());
        f.compare()?;
    }
    let inconsistent = match stamp(user("inconsistent", Some("second"), "body"), 3) {
        SessionMutation::Entry {
            seq,
            timestamp,
            lane,
            mut entry,
        } => {
            entry.base_mut().seq = 999;
            SessionMutation::Entry {
                seq,
                timestamp,
                lane,
                entry,
            }
        }
        _ => unreachable!(),
    };
    assert_error(
        f.store.append(&inconsistent, None, Fault::None),
        "inconsistent entry envelope",
    );
    f.compare()?;
    Ok(())
}

#[test]
fn real_native_history_growth_does_not_decode_retired_bodies() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut measured = Vec::new();
    for count in [128, 2048] {
        let path = directory.path().join(format!("native-{count}.rpidb"));
        let mut store = Store::create(&path, identity())?;
        let mut parent = None;
        for index in 0..count {
            let id = format!("entry-{index:06}");
            store.append(
                &stamp(
                    user(&id, parent.as_deref(), &"body".repeat(4096)),
                    index + 1,
                ),
                None,
                Fault::None,
            )?;
            parent = Some(id);
        }
        let mut large_start = start("active-run");
        if let SessionMutation::Record {
            record: LaneRecord::OperationStarted(r),
        } = &mut large_start
        {
            if let OperationIntent::Run {
                original_prompt, ..
            } = &mut r.intent
            {
                original_prompt
                    .push(rpi_ai::types::UserMessage::new("secret prompt".repeat(2048), 42).into());
            }
        }
        store.append(&stamp(large_start, count + 1), None, Fault::None)?;
        store.append(
            &stamp(
                enqueue(
                    "active-enqueue",
                    "main",
                    json!({"id":"future","body":"queue-body".repeat(4096)}),
                    "followUp",
                ),
                count + 2,
            ),
            None,
            Fault::None,
        )?;
        // Advance into a checkpoint containing active references, then leave an equal tail.
        for seq in count + 3..=count + TAIL {
            store.append(
                &SessionMutation::FactLabel {
                    seq,
                    target_id: "entry-000000".into(),
                    label: Some("annotation".repeat(4096)),
                },
                None,
                Fault::None,
            )?;
        }
        for seq in count + TAIL + 1..=count + TAIL + 2 {
            store.append(&stamp(record(json!({"type":"retry_pending","id":format!("retry-{}",seq-count),"runId":"active-run","attempt":1,"resultEntryId":"result"})),seq),None,Fault::None)?;
        }
        drop(store);
        let mut reopened = Store::open(&path, identity())?;
        reopened.forbid_bodies(true);
        let (state, reads) = reopened.restore()?;
        assert_eq!(reads.tail_rows, 2);
        assert_eq!(reads.reference_rows, 3); // head, active run, pending queue
        assert!(reopened.mutation(count + 1).is_err());
        assert_eq!(state.open["main"].len(), 1);
        assert_eq!(state.pending["main"].len(), 1);
        reopened.forbid_bodies(false);
        assert_eq!(reopened.open_operations("main")?.len(), 1);
        assert_eq!(reopened.pending("main")?.len(), 1);
        eprintln!("native_fact_restore_probe history_rows={} checkpoint_bytes={} tail_rows={} tail_bytes={} reference_rows={}",state.seq,reads.checkpoint_bytes,reads.tail_rows,reads.tail_bytes,reads.reference_rows);
        measured.push(reads);
    }
    // Decimal sequence width may grow; two metadata rows are still bounded.
    assert!(measured[0].tail_bytes.abs_diff(measured[1].tail_bytes) <= 8);
    assert!(
        measured[0]
            .checkpoint_bytes
            .abs_diff(measured[1].checkpoint_bytes)
            < 32
    );
    Ok(())
}

#[test]
fn native_body_damage_is_lazy_but_metadata_index_damage_is_rejected() -> Result<()> {
    let mut f = Fixture::new()?;
    for i in 0..20 {
        f.append(SessionMutation::FactName {
            seq: 0,
            name: Some(format!("name-{i}")),
        })?;
    }
    edit_database(&f.path, "UPDATE facts SET body='damaged' WHERE seq=1");
    f.reopen()?;
    assert_error(f.store.mutation(1), "native body digest mismatch");
    assert_eq!(f.store.name()?, Some("name-19".into()));
    edit_database(&f.path, "UPDATE facts SET category='entry' WHERE seq=20");
    assert_error(f.store.restore(), "native metadata index mismatch");
    Ok(())
}

#[test]
fn metadata_contains_only_projection_not_large_payloads() -> Result<()> {
    for mutation in [
        user("entry", None, &"never-copy-body".repeat(4096)),
        enqueue(
            "queue",
            "main",
            json!({"id":"future","body":"never-copy-queue".repeat(4096)}),
            "steer",
        ),
        record(
            json!({"type":"tool_started","id":"tool","runId":"run","assistantEntryId":"assistant","toolIndex":0,"toolCallId":"call","toolName":"fixture","effectiveArgs":{"body":"never-copy-tool".repeat(4096)},"resultEntryId":"result","replay":"safe"}),
        ),
    ] {
        let metadata = serde_json::to_string(&Meta::from_mutation(&stamp(mutation, 1))?)?;
        assert!(metadata.len() < 256);
        assert!(!metadata.contains("never-copy"));
    }
    Ok(())
}

fn bound_digest(seq: u64, domain: &str, bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&(native::FORMAT, identity(), seq, domain)).unwrap());
    digest.update(bytes);
    digest.finalize().to_vec()
}
fn rewrite_checkpoint(path: &std::path::Path, change: impl FnOnce(&mut Value)) -> Result<()> {
    let db = rusqlite::Connection::open(path)?;
    let original: String = db.query_row("SELECT json FROM checkpoint", [], |r| r.get(0))?;
    let mut value: Value = serde_json::from_str(&original)?;
    change(&mut value);
    let json = serde_json::to_string(&value)?;
    db.execute(
        "UPDATE checkpoint SET json=?1,digest=?2",
        rusqlite::params![json, bound_digest(0, "checkpoint", json.as_bytes())],
    )?;
    Ok(())
}
fn checkpoint_fixture() -> Result<Fixture> {
    let mut f = Fixture::new()?;
    f.append(user("head", None, "body"))?;
    f.append(start("active-run"))?;
    f.append(enqueue(
        "active-queue",
        "main",
        json!({"id":"future"}),
        "steer",
    ))?;
    while f.seq % TAIL != 0 {
        f.append(SessionMutation::FactName {
            seq: 0,
            name: Some("name".into()),
        })?;
    }
    Ok(f)
}
#[test]
fn checkpoint_active_references_are_validated_against_indexed_facts() -> Result<()> {
    for scenario in 0..7 {
        let mut f = checkpoint_fixture()?;
        rewrite_checkpoint(&f.path, |value| match scenario {
            0 => value["projection"]["open"]["main"][0]["id"] = json!("different-run"),
            1 => value["projection"]["pending"]["main"][0]["target"] = json!("wrong-target"),
            2 => value["projection"]["name"] = json!(1),
            3 => value["projection"]["heads"]["main"] = json!("active-run"),
            4 => {
                value["projection"]["open"]["absent"] = value["projection"]["open"]["main"].clone()
            }
            5 => value["identity"]["generation"] = json!("another-generation"),
            _ => value["projection"]["seq"] = json!(999),
        })?;
        assert!(
            f.store.restore().is_err(),
            "must reject scenario {scenario}"
        );
        assert!(Store::open(&f.path, identity()).is_err());
    }
    Ok(())
}
#[test]
fn canonical_body_and_metadata_disagreement_is_detected_even_with_correct_body_digest() -> Result<()>
{
    let mut f = Fixture::new()?;
    f.append(user("head", None, "body"))?;
    let changed = stamp(
        entry(
            "head",
            "custom",
            Some("main"),
            None,
            json!({"customType":"substitution"}),
        ),
        1,
    );
    let body = rpi_harness::session::jsonl::encode_mutation(&changed);
    rusqlite::Connection::open(&f.path)?.execute(
        "UPDATE facts SET body=?1,body_digest=?2 WHERE seq=1",
        rusqlite::params![body, bound_digest(1, "body", body.as_bytes())],
    )?;
    f.store.restore()?; // lazy body validation is deliberate
    assert_error(f.store.mutation(1), "native body metadata mismatch");
    Ok(())
}
#[test]
fn native_checkpoint_publication_error_rolls_back_facts_and_revision() -> Result<()> {
    let mut f = Fixture::new()?;
    for _ in 0..TAIL - 1 {
        f.append(SessionMutation::FactName { seq: 0, name: None })?;
    }
    let before = f.store.restore()?;
    edit_database(&f.path,"CREATE TRIGGER reject_checkpoint BEFORE UPDATE ON checkpoint BEGIN SELECT RAISE(ABORT,'native checkpoint refused'); END");
    let mutation = stamp(start("not-committed"), TAIL);
    assert_error(
        f.store.append(&mutation, None, Fault::None),
        "native checkpoint refused",
    );
    assert_eq!(f.store.restore()?, before);
    assert!(!f.store.reconcile(&mutation)?);
    edit_database(&f.path, "DROP TRIGGER reject_checkpoint");
    f.append(start("committed"))?;
    assert_eq!(f.store.restore()?.1.tail_rows, 0);
    Ok(())
}
#[test]
fn native_root_tail_and_digest_guards_fail_closed() -> Result<()> {
    for (sql, reason) in [
        (
            "UPDATE checkpoint SET digest=zeroblob(32)",
            "native checkpoint digest mismatch",
        ),
        (
            "UPDATE facts SET metadata_digest=zeroblob(32) WHERE seq=2",
            "native metadata digest mismatch",
        ),
        (
            "UPDATE facts SET lane=printf('%20000s','x') WHERE seq=2",
            "native metadata index budget exceeded",
        ),
        (
            "UPDATE facts SET metadata=printf('%20000s','x') WHERE seq=2",
            "native metadata budget exceeded",
        ),
        (
            "DELETE FROM facts WHERE seq=2",
            "nonconsecutive native metadata",
        ),
        ("UPDATE root SET revision=17", "native tail budget exceeded"),
        (
            "UPDATE root SET revision=2",
            "native history revision mismatch",
        ),
        (
            "UPDATE root SET generation='foreign'",
            "native root identity mismatch",
        ),
        ("PRAGMA user_version=9", "unsupported native fact format"),
    ] {
        let mut f = Fixture::new()?;
        for index in 0..3 {
            f.append(SessionMutation::FactName {
                seq: 0,
                name: Some(format!("name-{index}")),
            })?;
        }
        edit_database(&f.path, sql);
        let before = std::fs::read(&f.path)?;
        assert_error(Store::open(&f.path, identity()), reason);
        assert_eq!(std::fs::read(&f.path)?, before);
    }
    Ok(())
}

#[test]
fn native_store_never_adopts_foreign_artifacts_or_overwrites_existing_files() -> Result<()> {
    let f = Fixture::new()?;
    let bytes = std::fs::read(&f.path)?;
    assert!(Store::create(&f.path, identity()).is_err());
    assert_eq!(std::fs::read(&f.path)?, bytes);
    for field in ["session", "generation"] {
        let mut other = identity();
        if field == "session" {
            other.session = "foreign".into();
        } else {
            other.generation = "foreign".into();
        }
        assert_error(Store::open(&f.path, other), "native root identity mismatch");
    }
    let legacy = f._directory.path().join("legacy.jsonl");
    std::fs::write(&legacy, b"{\"kind\":\"header\",\"version\":4}\n")?;
    assert!(Store::open(&legacy, identity()).is_err());
    assert_eq!(
        std::fs::read(&legacy)?,
        b"{\"kind\":\"header\",\"version\":4}\n"
    );
    let old = f._directory.path().join("phase2.rpidb");
    rusqlite::Connection::open(&old)?
        .execute_batch("PRAGMA application_id=1380993360;PRAGMA user_version=1")?;
    assert_error(
        Store::open(&old, identity()),
        "unsupported native fact format",
    );
    let missing = f._directory.path().join("missing.rpidb");
    assert!(Store::open(&missing, identity()).is_err());
    assert!(!missing.exists());
    Ok(())
}

#[test]
fn validator_corruption_categories_survive_native_storage_roundtrip() -> Result<()> {
    use rpi_harness::session::{validate_record_log, RecordLogCorruptionReason, RecordLogSlice};
    let mut f = Fixture::new()?;
    f.append(start("run"))?;
    f.append(record(json!({"type":"step_attempt","id":"step-1","runId":"run","step":"assistant","attempt":1,"resultEntryId":"result-1"})))?;
    f.append(record(json!({"type":"step_attempt","id":"step-3","runId":"run","step":"assistant","attempt":3,"resultEntryId":"result-3"})))?;
    let oracle = f.oracle.find_records(&RecordQuery {
        lane: Some("main".into()),
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    })?;
    let stored = (1..=f.seq)
        .map(|seq| match f.store.mutation(seq)? {
            SessionMutation::Record { record } => Ok(record),
            _ => Err("expected native record".into()),
        })
        .collect::<Result<Vec<_>>>()?;
    let input = |records| RecordLogSlice {
        lane: "main".into(),
        open_operations: Vec::new(),
        records,
        entries: Vec::new(),
    };
    let left = validate_record_log(&input(oracle)).unwrap_err();
    let right = validate_record_log(&input(stored)).unwrap_err();
    assert_eq!(left, right);
    assert_eq!(
        right.reason,
        RecordLogCorruptionReason::NonConsecutiveAttempt
    );
    Ok(())
}
