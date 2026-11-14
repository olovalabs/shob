//! Mirrors `packages/agent/src/harness/session/memory.ts` — the in-memory
//! `SessionStorage` + `SessionRepo` back-ends used as the primary test backend
//! and as the reference for the JSONL backend.
//!
//! ## Adaptation notes
//!
//! - **Sharing**: the [`SessionStorage`] trait is `&self`, so a storage handed
//!   out by the repo must be shareable. [`InMemorySessionStorage`] wraps its
//!   mutable state in `Arc<Inner>` and is `Clone` (a cheap Arc bump), so the
//!   repo stores values directly and returns clones — the Rust analog of TS
//!   handing the same `Session` instance to multiple callers.
//! - **Stamping**: TS `appendEntry`/`appendRecord` take provisioned/new shapes
//!   and stamp `seq`+`timestamp` inline before calling `applyMutation`. The
//!   Rust [`SessionStorage`] trait takes a fully-stamped [`Entry`]/[`LaneRecord`];
//!   this backend stamps them itself via the state's `next_sequence` + a
//!   [`Clock`], then delegates to [`SessionState::apply_mutation`]. The state's
//!   consecutive-seq invariant stays authoritative across backends.
//! - **At-most-one open op**: TS `InMemorySessionStorage.appendRecord` rejects a
//!   2nd `operation_started` on a lane that already has an open op (a
//!   storage-level precondition distinct from the reducer-level corruption
//!   check). We mirror that here via [`SessionState::find_open_operations`]
//!   `limit:1`. The reducer still flags two-open as corruption for *recovery*.
//! - **Defensive copies**: reads clone (TS `structuredClone`); the state stores
//!   owned `Entry`/`LaneRecord` so `clone()` is the defensive copy.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{SessionError, SessionResult};
use crate::session::state::{ApplyOutcome, SessionState};
use crate::session::types::{
    provisioned_into_entry, BranchBounds, Entry, EntryQuery, ForkOptions, IdGenerator, LanePointer,
    LaneRecord, LogItem, LogOptions, OperationStartedRecord, ProvisionedEntry, RecordQuery,
    SessionCreateOptions, SessionLaneBranch, SessionMetadata, SessionMutation, SessionRepo,
    SessionStats, SessionStorage,
};

/// A simple shared id-generator producing stringified u64 counters — the
/// in-memory tests' deterministic generator. Production paths use uuidv7 via
/// the JSONL repo. Exposed so integration tests across the harness can share
/// one generator implementation.
#[derive(Debug, Default)]
pub struct CounterIdGenerator {
    n: Mutex<u64>,
}

impl CounterIdGenerator {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdGenerator for CounterIdGenerator {
    fn next(&self) -> String {
        let mut x = self.n.lock().expect("counter id generator not poisoned");
        *x += 1;
        format!("id-{x}")
    }
}

/// Wall-clock timestamp source — abstracted so tests can inject a fixed clock.
/// Production uses [`SystemClock`]; tests use [`FakeClock`] for deterministic
/// `timestamp` fields.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Real system clock — `SystemTime::now().duration_since(UNIX_EPOCH)` as ms.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Deterministic clock for tests — returns an incrementing ms counter.
#[derive(Debug, Default)]
pub struct FakeClock {
    ms: Mutex<i64>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self { ms: Mutex::new(0) }
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        let mut m = self.ms.lock().expect("fake clock not poisoned");
        *m += 1;
        *m
    }
}

/// Mutable inner state shared across clones of an [`InMemorySessionStorage`].
struct Inner {
    state: Mutex<SessionState>,
    progress: Mutex<super::progress::ProgressState>,
    clock: Arc<dyn Clock>,
    /// Reserved for future auto-id generation (callers currently supply entry/
    /// record ids). Kept so the constructor signature matches the JSONL repo's
    /// uuidv7-backed storage and stays drop-in replaceable.
    #[allow(dead_code)]
    ids: Arc<dyn IdGenerator>,
}

impl Inner {
    fn with_state<R>(
        &self,
        f: impl FnOnce(&mut SessionState) -> SessionResult<R>,
    ) -> SessionResult<R> {
        let mut state = self.state.lock().expect("state not poisoned");
        f(&mut state)
    }

    fn read_state<R>(&self, f: impl FnOnce(&SessionState) -> R) -> R {
        let state = self.state.lock().expect("state not poisoned");
        f(&state)
    }
}

/// In-memory `SessionStorage`. Owns the metadata + shared [`Inner`] and is
/// `Clone` (Arc bump), so the repo hands out clones and multiple holders share
/// the same durable state. Mirrors TS `InMemorySessionStorage`.
#[derive(Clone)]
pub struct InMemorySessionStorage {
    metadata: SessionMetadata,
    inner: Arc<Inner>,
}

impl InMemorySessionStorage {
    /// Build with a given metadata + clock + id generator. Production paths
    /// use [`SystemClock`] + uuidv7; tests inject deterministic doubles.
    pub fn new(
        metadata: SessionMetadata,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            metadata,
            inner: Arc::new(Inner {
                state: Mutex::new(SessionState::new()),
                progress: Mutex::new(Default::default()),
                clock,
                ids,
            }),
        }
    }

    /// Return a defensive snapshot of the reducer state. Used by durable
    /// backends that persist the same session semantics in another store.
    pub fn snapshot_state(&self) -> SessionState {
        self.inner.read_state(Clone::clone)
    }

    pub fn replace_state(&self, state: SessionState) {
        *self.inner.state.lock().expect("state not poisoned") = state;
    }

    /// Restore an in-memory storage from a previously persisted reducer state.
    pub fn from_state(
        metadata: SessionMetadata,
        state: SessionState,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            metadata,
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                progress: Mutex::new(Default::default()),
                clock,
                ids,
            }),
        }
    }

    fn append_entry_checked(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
        expected: Option<u64>,
    ) -> SessionResult<Entry> {
        self.inner.with_state(|state| {
            if let Some(expected) = expected {
                state.check_revision(expected)?;
            }
            let parent_id = state.require_lane(lane)?;
            state.validate_unused_id(&entry.id)?;
            let seq = state.next_sequence();
            let timestamp = self.inner.clock.now_ms();
            let full = provisioned_into_entry(entry, seq, parent_id, timestamp);
            match state.apply_mutation(SessionMutation::Entry {
                seq,
                timestamp,
                lane: Some(lane.to_string()),
                entry: full,
            })? {
                ApplyOutcome::Entry(stamped) => Ok(stamped),
                _ => Err(SessionError::storage("append_entry did not persist")),
            }
        })
    }

    fn append_record_checked(
        &self,
        record: LaneRecord,
        expected: Option<u64>,
    ) -> SessionResult<LaneRecord> {
        let lane = record.lane().to_string();
        let is_operation_started = matches!(record, LaneRecord::OperationStarted(_));
        self.inner.with_state(|state| {
            if let Some(expected) = expected {
                state.check_revision(expected)?;
            }
            state.require_lane(&lane)?;
            state.validate_unused_id(record.id())?;
            if is_operation_started {
                if let Some(current) = state.find_open_operations(&lane, Some(1))?.first() {
                    return Err(SessionError::storage(format!(
                        "Lane {} already has an open operation {}",
                        lane, current.base.id
                    )));
                }
            }
            let seq = state.next_sequence();
            let timestamp = self.inner.clock.now_ms();
            let stamped = stamp_record(record, seq, lane, timestamp);
            match state.apply_mutation(SessionMutation::Record { record: stamped })? {
                ApplyOutcome::Record(stamped) => Ok(stamped),
                _ => Err(SessionError::storage("append_record did not persist")),
            }
        })
    }

    /// Fork from a source storage — mirrors TS `fork`. Applies the source
    /// state's [`SessionState::create_fork_mutations`] into a fresh backend.
    pub fn fork_from(
        &self,
        metadata: SessionMetadata,
        options: &ForkOptions,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        let forked = InMemorySessionStorage::new(metadata, clock, ids);
        let mutations = self
            .inner
            .read_state(|state| state.create_fork_mutations(options))?;
        forked.inner.with_state(|state| {
            for mutation in mutations {
                let _ = state.apply_mutation(mutation)?;
            }
            Ok(())
        })?;
        Ok(forked)
    }
}

#[async_trait]
impl SessionStorage for InMemorySessionStorage {
    async fn prune_completed_progress(&self) -> SessionResult<usize> {
        let completed = self
            .inner
            .with_state(|state| Ok(state.completed_progress_runs()))?;
        Ok(self
            .inner
            .progress
            .lock()
            .unwrap()
            .prune_completed(&completed))
    }

    async fn append_progress(
        &self,
        records: Vec<super::types::AssistantFrameRecord>,
    ) -> SessionResult<Vec<super::types::AssistantFrameRecord>> {
        self.inner.with_state(|state| {
            for record in &records {
                state.require_lane(&record.base.lane)?;
            }
            Ok(())
        })?;
        let mut progress = self.inner.progress.lock().unwrap();
        let records = progress.prepare(records, || self.inner.clock.now_ms())?;
        progress.apply(&records);
        Ok(records)
    }
    async fn read_progress(
        &self,
        run_id: &str,
    ) -> SessionResult<Vec<super::types::AssistantFrameRecord>> {
        Ok(self.inner.progress.lock().unwrap().read(run_id))
    }
    fn metadata(&self) -> SessionMetadata {
        self.metadata.clone()
    }

    fn is_ephemeral(&self) -> bool {
        true
    }

    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        Ok(self.inner.read_state(|state| state.get_lanes()))
    }

    async fn lane_branch(&self, lane: &str) -> SessionResult<SessionLaneBranch> {
        self.inner.read_state(|state| state.lane_branch(lane))
    }

    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.inner.with_state(|state| {
            state.validate_new_lane(lane)?;
            state.validate_target(at)?;
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::Lane {
                seq,
                lane: lane.to_string(),
                leaf_id: at.map(|s| s.to_string()),
            })?;
            Ok(())
        })
    }

    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.inner.with_state(|state| {
            state.require_lane(lane)?;
            state.validate_target(to)?;
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::Lane {
                seq,
                lane: lane.to_string(),
                leaf_id: to.map(|s| s.to_string()),
            })?;
            Ok(())
        })
    }

    async fn append_queue_cancellations(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        self.inner.with_state(|state| {
            state.validate_queue_cancellations(&records)?;
            let mut committed = Vec::with_capacity(records.len());
            for record in records {
                let lane = record.lane().to_owned();
                let record = stamp_record(
                    record,
                    state.next_sequence(),
                    lane,
                    self.inner.clock.now_ms(),
                );
                if let ApplyOutcome::Record(record) =
                    state.apply_mutation(SessionMutation::Record { record })?
                {
                    committed.push(record);
                }
            }
            Ok(committed)
        })
    }

    async fn history_revision(&self) -> SessionResult<u64> {
        Ok(self.inner.read_state(SessionState::sequence))
    }

    async fn append_entry_if_revision(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
        expected: u64,
    ) -> SessionResult<Entry> {
        self.append_entry_checked(entry, lane, Some(expected))
    }

    async fn append_record_if_revision(
        &self,
        record: LaneRecord,
        expected: u64,
    ) -> SessionResult<LaneRecord> {
        self.append_record_checked(record, Some(expected))
    }

    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.append_entry_checked(entry, lane, None)
    }

    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.append_record_checked(record, None)
    }

    async fn message_summaries(
        &self,
        lane: &str,
        query: &crate::session::types::MessageSummaryQuery,
    ) -> SessionResult<crate::session::types::MessageSummaryPage> {
        self.inner
            .read_state(|state| state.message_summaries(lane, query))
    }
    async fn message_window(
        &self,
        lane: &str,
        query: &crate::session::types::MessageWindowQuery,
    ) -> SessionResult<crate::session::types::MessageWindowPage> {
        self.inner
            .read_state(|state| state.message_window(lane, query))
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        Ok(self.inner.read_state(|state| state.get_entry(id).cloned()))
    }

    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        self.inner.read_state(|state| state.find_entries(query))
    }

    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        self.inner
            .read_state(|state| state.find_entries_on_branch(query, bounds, start))
    }

    async fn branch_ancestry(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<crate::session::types::EntryAncestry>> {
        self.inner
            .read_state(|state| state.branch_ancestry(bounds, start))
    }

    async fn context_on_branch(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<crate::session::context::SessionContext> {
        self.inner
            .read_state(|state| state.context_on_branch(bounds, start))
    }

    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        self.inner.read_state(|state| state.find_records(query))
    }

    async fn record_page(
        &self,
        query: &crate::session::types::RecordPageQuery,
    ) -> SessionResult<crate::session::types::RecordPage> {
        self.inner.read_state(|state| state.record_page(query))
    }

    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.inner
            .read_state(|state| state.find_open_operations(lane, limit))
    }

    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.inner.read_state(|state| state.get_log(options))
    }

    async fn get_name(&self) -> SessionResult<Option<String>> {
        Ok(self
            .inner
            .read_state(|state| state.get_name().map(|s| s.to_string())))
    }

    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.inner.with_state(|state| {
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::FactName {
                seq,
                name: name.map(|s| s.to_string()),
            })?;
            Ok(())
        })
    }

    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        Ok(self
            .inner
            .read_state(|state| state.get_label(id).map(|s| s.to_string())))
    }

    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        self.inner.with_state(|state| {
            state.validate_target(Some(id))?;
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::FactLabel {
                seq,
                target_id: id.to_string(),
                label: label.map(|s| s.to_string()),
            })?;
            Ok(())
        })
    }

    async fn get_stats(&self) -> SessionResult<SessionStats> {
        Ok(self.inner.read_state(|state| state.get_stats().clone()))
    }
}

/// Stamp a record's `seq`/`lane`/`timestamp` fields. The caller-built record may
/// carry only the intent-level fields; this rebuilds it with the storage-assigned
/// [`RecordBase`] fields set. Mirrors TS `{ ...newRecord, seq, timestamp }`.
///
/// Shared by both in-memory and JSONL storage backends (each stamps `seq` +
/// `timestamp` from its own clock + state before persisting), hence `pub(crate)`.
pub(crate) fn stamp_record(
    record: LaneRecord,
    seq: u64,
    lane: String,
    timestamp: i64,
) -> LaneRecord {
    use crate::session::types::*;
    let stamp = |mut base: RecordBase| {
        base.seq = seq;
        base.lane = lane.clone();
        base.timestamp = timestamp;
        base
    };
    match record {
        LaneRecord::OperationStarted(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::OperationStarted(r)
        }
        LaneRecord::AbortRequested(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::AbortRequested(r)
        }
        LaneRecord::OperationFinished(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::OperationFinished(r)
        }
        LaneRecord::StepAttempt(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::StepAttempt(r)
        }
        LaneRecord::ToolStarted(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::ToolStarted(r)
        }
        LaneRecord::QueueEnqueued(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::QueueEnqueued(r)
        }
        LaneRecord::QueueCancelled(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::QueueCancelled(r)
        }
        LaneRecord::WriteDeferred(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::WriteDeferred(r)
        }
        LaneRecord::Usage(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::Usage(r)
        }
        LaneRecord::AssistantFrame(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::AssistantFrame(r)
        }
        LaneRecord::RetryPending(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::RetryPending(r)
        }
    }
}

/// In-memory `SessionRepo`. Owns all sessions in a `HashMap` keyed by id and
/// hands out `Clone`d [`InMemorySessionStorage`] handles (Arc bumps). Mirrors
/// TS `InMemorySessionRepo`.
pub struct InMemorySessionRepo {
    sessions: Mutex<HashMap<String, InMemorySessionStorage>>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
}

impl InMemorySessionRepo {
    pub fn new(clock: Arc<dyn Clock>, ids: Arc<dyn IdGenerator>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            clock,
            ids,
        }
    }

    fn require_storage(&self, id: &str) -> SessionResult<InMemorySessionStorage> {
        let sessions = self.sessions.lock().expect("sessions not poisoned");
        sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::not_found(format!("Session not found: {id}")))
    }
}

#[async_trait]
impl SessionRepo for InMemorySessionRepo {
    type Storage = InMemorySessionStorage;

    async fn create(&self, options: &SessionCreateOptions) -> SessionResult<Self::Storage> {
        let id = options.id.clone().unwrap_or_else(|| self.ids.next());
        let mut sessions = self.sessions.lock().expect("sessions not poisoned");
        if sessions.contains_key(&id) {
            return Err(SessionError::already_exists(format!(
                "Session already exists: {id}"
            )));
        }
        let metadata = SessionMetadata {
            id: id.clone(),
            created_at: self.clock.now_ms(),
            parent_session_id: options.parent_session_id.clone(),
        };
        let storage = InMemorySessionStorage::new(metadata, self.clock.clone(), self.ids.clone());
        sessions.insert(id, storage.clone());
        Ok(storage)
    }

    async fn open(&self, metadata: &SessionMetadata) -> SessionResult<Self::Storage> {
        self.require_storage(&metadata.id)
    }

    async fn list(&self) -> SessionResult<Vec<SessionMetadata>> {
        let sessions = self.sessions.lock().expect("sessions not poisoned");
        Ok(sessions.values().map(|s| s.metadata()).collect())
    }

    async fn delete(&self, metadata: &SessionMetadata) -> SessionResult<()> {
        let mut sessions = self.sessions.lock().expect("sessions not poisoned");
        sessions.remove(&metadata.id);
        Ok(())
    }

    async fn fork(
        &self,
        source: &SessionMetadata,
        options: &SessionCreateOptions,
        fork: &ForkOptions,
    ) -> SessionResult<Self::Storage> {
        let source_storage = self.require_storage(&source.id)?;
        let id = options.id.clone().unwrap_or_else(|| self.ids.next());
        let mut sessions = self.sessions.lock().expect("sessions not poisoned");
        if sessions.contains_key(&id) {
            return Err(SessionError::already_exists(format!(
                "Session already exists: {id}"
            )));
        }
        let metadata = SessionMetadata {
            id: id.clone(),
            created_at: self.clock.now_ms(),
            parent_session_id: options
                .parent_session_id
                .clone()
                .or_else(|| Some(source.id.clone())),
        };
        let forked =
            source_storage.fork_from(metadata, fork, self.clock.clone(), self.ids.clone())?;
        sessions.insert(id, forked.clone());
        Ok(forked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::{
        EntryOrder, OperationIntent, OperationStartedRecord, ProvisionedKind, RecordBase,
    };

    fn counter_ids() -> Arc<dyn IdGenerator> {
        Arc::new(CounterIdGenerator::new())
    }

    fn fresh_storage() -> InMemorySessionStorage {
        InMemorySessionStorage::new(
            SessionMetadata {
                id: "s1".to_string(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            counter_ids(),
        )
    }

    fn rec_base(id: &str, seq: u64, lane: &str) -> RecordBase {
        RecordBase {
            id: id.to_string(),
            seq,
            lane: lane.to_string(),
            timestamp: seq as i64,
        }
    }

    fn user_msg(text: &str) -> rpi_agent::message::AgentMessage {
        rpi_agent::message::AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
    }

    #[tokio::test]
    async fn in_memory_context_projection_matches_branch_builder_and_is_independent() {
        let storage = fresh_storage();
        for (id, message) in [("e1", "old"), ("e2", "visible"), ("e3", "latest")] {
            storage
                .append_entry(
                    ProvisionedEntry {
                        id: id.into(),
                        kind: ProvisionedKind::Message {
                            message: user_msg(message),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
        }
        let bounds = BranchBounds {
            stop_at_id: Some("e2".into()),
            ..Default::default()
        };
        let projected = storage.context_on_branch(&bounds, "e3").await.unwrap();
        let path = storage
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &bounds,
                "e3",
            )
            .await
            .unwrap();
        let expected = crate::session::context::build_session_context(&path, &Default::default());
        assert_eq!(projected.messages, expected.messages);
        assert_eq!(projected.thinking_level, expected.thinking_level);
        let mut modified = projected;
        modified.messages.clear();
        assert_eq!(
            storage
                .context_on_branch(&bounds, "e3")
                .await
                .unwrap()
                .messages
                .len(),
            expected.messages.len()
        );
    }

    #[tokio::test]
    async fn in_memory_roundtrip_entry_and_record() {
        let storage = fresh_storage();
        let entry = storage
            .append_entry(
                ProvisionedEntry {
                    id: "e1".to_string(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("hello"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .expect("append entry");
        assert_eq!(entry.id(), "e1");
        assert_eq!(entry.seq(), 1);
        assert_eq!(entry.parent_id(), None);

        let got = storage
            .get_entry("e1")
            .await
            .unwrap()
            .expect("entry exists");
        assert_eq!(got.id(), "e1");
    }

    #[tokio::test]
    async fn in_memory_rejects_second_open_operation() {
        let storage = fresh_storage();
        let started = LaneRecord::OperationStarted(OperationStartedRecord {
            base: rec_base("op-1", 0, "main"),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        });
        storage.append_record(started).await.expect("first op");

        let second = LaneRecord::OperationStarted(OperationStartedRecord {
            base: rec_base("op-2", 0, "main"),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        });
        let err = storage.append_record(second).await.unwrap_err();
        assert!(err.to_string().contains("already has an open operation"));
    }

    #[tokio::test]
    async fn in_memory_fork_branch_copies_entries() {
        let storage = fresh_storage();
        storage
            .append_entry(
                ProvisionedEntry {
                    id: "e1".to_string(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("hi"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();

        let forked = storage
            .fork_from(
                SessionMetadata {
                    id: "s2".to_string(),
                    created_at: 0,
                    parent_session_id: Some("s1".to_string()),
                },
                &ForkOptions::default(),
                Arc::new(FakeClock::new()),
                counter_ids(),
            )
            .expect("fork");
        let entries = forked.find_entries(&Default::default()).await.unwrap();
        assert!(entries.iter().any(|e| e.id() == "e1"));
    }
}
