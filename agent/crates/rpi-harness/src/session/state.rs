//! Mirrors `packages/agent/src/harness/session/state.ts` — the in-memory
//! mutation reducer that enforces the recoverable-session invariants.
//!
//! Critical invariants enforced here (plan §5):
//! - **Consecutive seq**: `seq == state.sequence + 1` or `invalid_entry`.
//!   Recovery from a torn tail relies on a global monotonic sequence.
//! - **Write-once ids**: entry/record ids are unique forever (`already_exists`
//!   on reuse).
//! - **Lane-leaf chaining**: an entry with `lane: Some(l)` must have
//!   `entry.parent_id == lanes[l]` (the lane's current leaf). Otherwise the
//!   branch would fork silently.
//! - **Fork-without-lane**: the fork path passes `lane: None`, so chaining is
//!   *not* checked — the forked entries arrive with their original `parent_id`
//!   chain intact, and a subsequent `Lane { lane, leaf_id }` mutation re-points
//!   the lane.
//! - **At-most-one open op per lane**: a 2nd `operation_started` while one is
//!   open is *not* rejected here (state allows the map to hold multiple, to
//!   support the "aborting while suspended" snapshot path); the **reducer**
//!   (`reducer.rs`) flags `multiple_open_operations` as corruption, and
//!   recovery's `find_open_operations(limit: 2)` interprets `>=2` as corruption.
//!
//! Note: the TS `applyMutation` allows multiple open operations to coexist in
//! `openOperationsByLane` (it only deletes on `operation_finished`). The
//! "at-most-one" rule is a *reducer-level* corruption check plus a harness-level
//! precondition, not a state-level hard reject. This port mirrors that exactly.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::error::{SessionError, SessionResult};
use crate::session::types::{
    AssistantFrameOp, BranchBounds, Entry, EntryOrder, EntryQuery, ForkOptions, ForkPosition,
    LanePointer, LaneRecord, LogItem, LogOptions, OperationStartedRecord, RecordQuery,
    SessionMetadata, SessionMutation, SessionStats,
};

mod index;
#[cfg(test)]
mod index_tests;
mod metadata;
#[cfg(test)]
mod record_page_tests;
mod selection;
mod snapshot;
pub(crate) use index::IndexedState;
pub(crate) use metadata::EntryMeta;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum StoredLogItem {
    Entry(usize),
    Record(usize),
    Other(Box<LogItem>),
}

/// The in-memory mutation reducer. Mirrors TS `SessionState`. Owns the
/// append-only entry/record logs, the lane→leaf map, open operations per lane,
/// global name/labels facts, and running stats.
#[derive(Debug, Clone)]
pub struct SessionState {
    index: IndexedState<Entry>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    /// Borrow record payloads; copy only terminal scope metadata once per cleanup.
    pub(crate) fn completed_progress_runs(&self) -> super::progress::CompletedProgressRuns {
        self.index.completed_progress_runs()
    }

    /// New state with a single `main` lane pointing at no leaf (mirrors TS
    /// `lanes = new Map([["main", null]])`).
    pub fn new() -> Self {
        Self {
            index: IndexedState::new(),
        }
    }

    pub fn next_sequence(&self) -> u64 {
        self.index.next_sequence()
    }

    /// The highest applied mutation seq (0 before any mutation). The JSONL
    /// loader uses this to detect a duplicated/late append, where a mutation
    /// arrives with an already-consumed seq.
    pub(crate) fn sequence(&self) -> u64 {
        self.index.sequence()
    }

    /// Validate the entire cancellation batch before any reducer mutation.
    pub(crate) fn validate_queue_cancellations(&self, records: &[LaneRecord]) -> SessionResult<()> {
        self.index.validate_queue_cancellations(records)
    }

    pub(crate) fn check_revision(&self, expected: u64) -> SessionResult<()> {
        self.index.check_revision(expected)
    }

    /// Advance the seq counter to `seq` (no-op when already at or past it).
    /// The JSONL loader calls this when a divergent writer's counter leaves a
    /// gap, so the next mutation is accepted instead of refused.
    #[cfg(test)]
    pub(crate) fn resync_sequence(&mut self, seq: u64) {
        self.index.resync_sequence(seq)
    }

    pub fn get_lanes(&self) -> Vec<LanePointer> {
        self.index.get_lanes()
    }

    /// Atomically snapshot the authoritative lane leaf and explicit navigation
    /// generation while the reducer is held by its storage backend.
    pub fn lane_branch(
        &self,
        lane: &str,
    ) -> SessionResult<crate::session::types::SessionLaneBranch> {
        let (leaf_id, generation) = self.index.lane_branch(lane)?;
        Ok(crate::session::types::SessionLaneBranch {
            lane: lane.to_owned(),
            leaf_id,
            generation,
        })
    }

    /// Look up a lane's leaf, or `invalid_lane` if the lane doesn't exist.
    pub fn require_lane(&self, lane: &str) -> SessionResult<Option<String>> {
        self.index.require_lane(lane)
    }

    /// `already_exists` if the lane already exists. Mirrors `validateNewLane`.
    pub fn validate_new_lane(&self, lane: &str) -> SessionResult<()> {
        self.index.validate_new_lane(lane)
    }

    /// `not_found` if `target_id` is set and unknown. Mirrors `validateTarget`.
    pub fn validate_target(&self, target_id: Option<&str>) -> SessionResult<()> {
        self.index.validate_target(target_id)
    }

    /// `already_exists` if the id was already used. Mirrors `validateUnusedId`.
    pub fn validate_unused_id(&self, id: &str) -> SessionResult<()> {
        self.index.validate_unused_id(id)
    }

    /// Apply a mutation, enforcing every invariant. Mirrors TS `applyMutation`.
    /// The `Entry` arm carries a full [`Entry`] whose `seq`/`parent_id`/`timestamp`
    /// are already stamped by the storage layer (TS shape); the state *validates*
    /// lane-leaf chaining against `entry.parent_id`, it does not set it.
    pub fn apply_mutation(&mut self, mutation: SessionMutation) -> SessionResult<ApplyOutcome> {
        self.index.apply_mutation(mutation, |entry| entry.clone())
    }

    pub fn get_entry(&self, id: &str) -> Option<&Entry> {
        self.index
            .entries_by_id
            .get(id)
            .map(|index| &self.index.entries[*index])
    }

    pub fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        let mut results = Vec::new();
        for index in self.index.select_entry_indices(query, None)? {
            let entry = &self.index.entries[index];
            if matches_entry_payload(entry, query) {
                results.push(entry.clone());
                if query.limit.is_some_and(|limit| results.len() >= limit) {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        let mut results = Vec::new();
        for index in self
            .index
            .select_entry_indices(query, Some((bounds, start)))?
        {
            let entry = &self.index.entries[index];
            if matches_entry_payload(entry, query) {
                results.push(entry.clone());
                if query.limit.is_some_and(|limit| results.len() >= limit) {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub fn branch_ancestry(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<crate::session::types::EntryAncestry>> {
        self.index.branch_ancestry(bounds, start)
    }

    /// Project a validated branch without allocating owned copies for entries
    /// that fall before the latest compaction boundary.
    pub(crate) fn context_on_branch(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<crate::session::context::SessionContext> {
        let diagnostics =
            std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0");
        let started = diagnostics.then(std::time::Instant::now);
        let mut path = self.walk_to_root(Some(start), bounds)?;
        path.reverse();
        if bounds.stop_at_id.is_some() || bounds.stop_at_type.is_some() {
            if let Some(end) = path.iter().position(|entry| {
                bounds.stop_at_id.as_deref() == Some(entry.id())
                    || bounds
                        .stop_at_type
                        .map(|kind| kind == entry.entry_type())
                        .unwrap_or(false)
            }) {
                path.truncate(end + 1);
            }
        }
        if let Some(started) = started {
            eprintln!(
                "RPI_CONTEXT_DIAGNOSTICS path_load_ns={} path_entries={}",
                started.elapsed().as_nanos().min(u64::MAX as u128),
                path.len(),
            );
        }
        Ok(crate::session::context::build_session_context_from_borrowed(&path))
    }

    pub fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        self.index.find_records(query)
    }

    pub fn record_page(
        &self,
        query: &crate::session::types::RecordPageQuery,
    ) -> SessionResult<crate::session::types::RecordPage> {
        self.index.record_page(query)
    }

    pub fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.index.find_open_operations(lane, limit)
    }

    pub fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        let mut results = Vec::new();
        for item in self.index.log_items(options)? {
            let seq = match item {
                StoredLogItem::Entry(index) => self.index.entries[*index].seq(),
                StoredLogItem::Record(index) => self.index.records[*index].seq(),
                StoredLogItem::Other(item) => item.seq(),
            };
            results.push(match item {
                StoredLogItem::Entry(index) => LogItem::Entry {
                    seq,
                    entry: self.index.entries[*index].clone(),
                },
                StoredLogItem::Record(index) => LogItem::Record {
                    seq,
                    record: self.index.records[*index].clone(),
                },
                StoredLogItem::Other(item) => (**item).clone(),
            });
            if let Some(limit) = options.limit {
                if results.len() >= limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub fn get_name(&self) -> Option<&str> {
        self.index.get_name()
    }

    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.index.get_label(id)
    }

    pub fn get_stats(&self) -> &SessionStats {
        self.index.get_stats()
    }

    /// Build the fork mutation list. Mirrors TS `createForkMutations`. The
    /// forked session replays these from `seq=1`. Forked entries keep their
    /// original `id` + `parent_id` chain and pass `lane: None` so lane-leaf
    /// chaining is NOT re-checked (invariant §7).
    pub fn create_fork_mutations(
        &self,
        options: &ForkOptions,
    ) -> SessionResult<Vec<SessionMutation>> {
        let (copied_entries, fork_lanes): (Vec<Entry>, Vec<LanePointer>) = match options {
            ForkOptions::Tree => {
                let entries = self.find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })?;
                let lanes = self.get_lanes();
                (entries, lanes)
            }
            ForkOptions::Branch { entry_id, position } => {
                let selected = match entry_id {
                    Some(id) => Some(id.clone()),
                    None => self.require_lane("main")?,
                };
                let target_id: Option<String> = if let Some(sel) = selected {
                    let entry = self.get_entry(&sel).ok_or_else(|| {
                        SessionError::not_found(format!("Entry not found: {sel}"))
                    })?;
                    // An omitted target denotes the actual current leaf, which
                    // may be a host/custom entry. Explicit fork targets retain
                    // the ordinary message-only contract.
                    if entry_id.is_some() && !matches!(entry, Entry::Message(_)) {
                        return Err(SessionError::invalid_fork_target(format!(
                            "Fork target is not a message entry: {sel}"
                        )));
                    }
                    let position = position.unwrap_or(match entry_id {
                        Some(_) => ForkPosition::Before,
                        None => ForkPosition::At,
                    });
                    match position {
                        ForkPosition::At => Some(entry.id().to_string()),
                        ForkPosition::Before => entry.parent_id().map(|s| s.to_string()),
                    }
                } else {
                    None
                };
                let copied = if let Some(target) = &target_id {
                    self.find_entries_on_branch(
                        &EntryQuery {
                            order: Some(EntryOrder::OldestFirst),
                            ..Default::default()
                        },
                        &BranchBounds::default(),
                        target,
                    )?
                } else {
                    Vec::new()
                };
                let lanes = vec![LanePointer {
                    lane: "main".into(),
                    leaf_id: target_id,
                }];
                (copied, lanes)
            }
        };

        let mut mutations = Vec::new();
        let mut sequence = 1u64;
        for source_entry in &copied_entries {
            // Re-stamp seq (timestamp preserved from source). Fork path: the
            // entry keeps its original id/parent_id; `lane: None` bypasses the
            // lane-leaf chaining check.
            let mut forked = source_entry.clone();
            forked.base_mut().seq = sequence;
            mutations.push(SessionMutation::Entry {
                seq: sequence,
                timestamp: source_entry.base().timestamp,
                lane: None,
                entry: forked,
            });
            sequence += 1;
        }
        for pointer in &fork_lanes {
            mutations.push(SessionMutation::Lane {
                seq: sequence,
                lane: pointer.lane.clone(),
                leaf_id: pointer.leaf_id.clone(),
            });
            sequence += 1;
        }
        if let Some(name) = &self.index.name {
            mutations.push(SessionMutation::FactName {
                seq: sequence,
                name: Some(name.clone()),
            });
            sequence += 1;
        }
        for entry in &copied_entries {
            if let Some(label) = self.index.labels.get(entry.id()) {
                mutations.push(SessionMutation::FactLabel {
                    seq: sequence,
                    target_id: entry.id().to_string(),
                    label: Some(label.clone()),
                });
                sequence += 1;
            }
        }
        Ok(mutations)
    }

    // ---- private helpers ----

    fn walk_to_root(
        &self,
        start: Option<&str>,
        bounds: &BranchBounds,
    ) -> SessionResult<Vec<&Entry>> {
        Ok(self
            .index
            .walk_to_root(start, bounds)?
            .into_iter()
            .map(|index| &self.index.entries[index])
            .collect())
    }
}

/// What `apply_mutation` returned — mirrors the TS pattern where each mutation
/// variant updates different state.
#[derive(Debug, Clone)]
pub enum ApplyOutcome {
    Entry(Entry),
    Record(LaneRecord),
    Lane,
    Fact,
}

/// Outcome of a fork: the metadata to stamp on the new session.
#[derive(Debug, Clone)]
pub struct ForkResult {
    pub metadata: SessionMetadata,
    pub mutations: Vec<SessionMutation>,
}

// ---- helpers ----

fn validate_limit(limit: Option<usize>) -> SessionResult<()> {
    if let Some(l) = limit {
        if l == 0 {
            return Err(SessionError::invalid_query(
                "limit must be a positive integer",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(dead_code)] // state introspection helpers for upcoming session_state tests
impl SessionState {
    pub(crate) fn lanes(&self) -> &BTreeMap<String, Option<String>> {
        &self.index.lanes
    }
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.index.entries
    }
}

mod message_queries;

/// Complete matcher for hydrated entries. Metadata candidates alone are not a
/// match when a tool/outbox predicate is present.
#[cfg(test)]
pub(crate) fn matches_entry_query(entry: &Entry, query: &EntryQuery) -> bool {
    EntryMeta::from_entry(entry).matches_query(query) && matches_entry_payload(entry, query)
}

/// Apply only predicates omitted by metadata selection, without cloning facts.
pub(crate) fn matches_entry_payload(entry: &Entry, query: &EntryQuery) -> bool {
    let outbox_ok = query.outbox_key.as_deref().is_none_or(|key| match entry {
        Entry::Message(message) => match &message.message {
            rpi_agent::AgentMessage::Custom(custom) => {
                custom
                    .data
                    .get("__rpiOutbox")
                    .and_then(|value| value.get("key"))
                    .and_then(serde_json::Value::as_str)
                    == Some(key)
            }
            _ => false,
        },
        _ => false,
    });
    let tool_ok = query.tool_call_id.as_deref().is_none_or(|id| match entry {
        Entry::Message(entry) => match &entry.message {
            rpi_agent::AgentMessage::Assistant(message) => message.content.iter().any(
                |block| matches!(block, rpi_ai::types::Content::ToolCall(call) if call.id == id),
            ),
            rpi_agent::AgentMessage::ToolResult(result) => result.tool_call_id == id,
            _ => false,
        },
        _ => false,
    });
    outbox_ok && tool_ok
}

impl<P: Clone> IndexedState<P> {
    /// Drop the frames a clear has retired: one per streamed delta, each a
    /// parsed JSON value, and never read again because recovery skips what a
    /// clear covers. Left in place they outweigh the transcript several times
    /// over. The session file keeps them, and a reload retires them here too.
    /// Their IDs stay in `used_ids`: the file still holds them, so they stay taken.
    fn retire_frames(&mut self, run_id: &str, stream_index: Option<usize>, before_seq: u64) {
        // Cold replay has no resident appends. Do not scan unrelated commit,
        // operation, or usage records for a retirement that cannot match.
        if self.hot_append_count == 0 {
            return;
        }
        let retired = |record: &LaneRecord| {
            matches!(record, LaneRecord::AssistantFrame(frame)
                if frame.op == AssistantFrameOp::Append
                    && frame.base.seq < before_seq
                    && frame.run_id == run_id
                    && stream_index.is_none_or(|index| frame.stream_index == index))
        };
        // Everything before the first retired frame keeps its index.
        let Some(first) = self.records.iter().position(retired) else {
            #[cfg(test)]
            {
                self.retire_record_checks += self.records.len();
            }
            return;
        };
        #[cfg(test)]
        {
            self.retire_record_checks += first + 1;
        }
        let mut moved = vec![None; self.records.len() - first];
        let mut kept = first;
        let tail = self.records.split_off(first);
        for (offset, record) in tail.into_iter().enumerate() {
            #[cfg(test)]
            {
                self.retire_record_checks += 1;
            }
            if !retired(&record) {
                moved[offset] = Some(kept);
                kept += 1;
                self.records.push(record);
            } else {
                self.hot_append_count -= 1;
            }
        }
        // The log lists records in order, so only its tail can name a moved one.
        let log_tail = self
            .log
            .iter()
            .rposition(|item| matches!(item, StoredLogItem::Record(index) if *index < first))
            .map_or(0, |at| at + 1);
        let mut position = 0;
        self.log.retain_mut(|item| {
            position += 1;
            if position <= log_tail {
                return true;
            }
            let StoredLogItem::Record(index) = item else {
                return true;
            };
            match moved[*index - first] {
                Some(now) => {
                    *index = now;
                    true
                }
                None => false,
            }
        });
    }
}

impl<P: Clone> IndexedState<P> {
    #[cfg(test)]
    pub(crate) fn retire_record_checks(&self) -> usize {
        self.retire_record_checks
    }

    pub(crate) fn retire_cleared_frames(
        &mut self,
        frame: &crate::session::types::AssistantFrameRecord,
    ) {
        match frame.op {
            AssistantFrameOp::ClearStream => {
                self.retire_frames(&frame.run_id, Some(frame.stream_index), frame.base.seq)
            }
            AssistantFrameOp::ClearRun => self.retire_frames(&frame.run_id, None, frame.base.seq),
            _ => {}
        }
    }
    pub(crate) fn take_payloads(&mut self) -> Vec<P> {
        std::mem::take(&mut self.entries)
    }
    /// Transfer validated metadata without replay or loss of retired ID reservations.
    pub(crate) fn into_session_state(
        mut self,
        entries: Vec<Entry>,
        cold_records: Vec<LaneRecord>,
    ) -> SessionState {
        assert_eq!(entries.len(), self.metadata.len());
        let original_seqs: Vec<_> = self.records.iter().map(LaneRecord::seq).collect();
        let cold_seqs: Vec<_> = cold_records.iter().map(LaneRecord::seq).collect();
        self.records.extend(cold_records);
        self.records.sort_unstable_by_key(LaneRecord::seq);
        self.hot_append_count = self
            .records
            .iter()
            .filter(|record| {
                matches!(record, LaneRecord::AssistantFrame(frame) if frame.op == AssistantFrameOp::Append)
            })
            .count();
        let positions: HashMap<_, _> = self
            .records
            .iter()
            .enumerate()
            .map(|(i, record)| (record.seq(), i))
            .collect();
        for item in &mut self.log {
            if let StoredLogItem::Record(index) = item {
                *index = positions[&original_seqs[*index]];
            }
        }
        self.log.extend(
            cold_seqs
                .into_iter()
                .map(|seq| StoredLogItem::Record(positions[&seq])),
        );
        self.log.sort_unstable_by_key(|item| match item {
            StoredLogItem::Entry(i) => entries[*i].seq(),
            StoredLogItem::Record(i) => self.records[*i].seq(),
            StoredLogItem::Other(item) => item.seq(),
        });
        SessionState {
            index: IndexedState {
                sequence: self.sequence,
                used_ids: self.used_ids,
                entries,
                metadata: self.metadata,
                entries_by_id: self.entries_by_id,
                nearest_user: self.nearest_user,
                records: self.records,
                hot_append_count: self.hot_append_count,
                #[cfg(test)]
                retire_record_checks: self.retire_record_checks,
                open_operations_by_lane: self.open_operations_by_lane,
                lanes: self.lanes,
                lane_generations: self.lane_generations,
                log: self.log,
                stats: self.stats,
                name: self.name,
                labels: self.labels,
            },
        }
    }
}
