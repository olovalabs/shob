//! Payload-independent session mutation reducer and derived metadata indexes.
use super::*;

#[derive(Debug, Clone)]
pub(crate) struct IndexedState<P: Clone> {
    pub(super) sequence: u64,
    pub(super) used_ids: std::collections::HashSet<String>,
    pub(super) entries: Vec<P>,
    pub(super) metadata: Vec<EntryMeta>,
    pub(super) entries_by_id: HashMap<String, usize>,
    // Rebuilt by replay, including imported/forked entries; never persisted separately.
    pub(super) nearest_user: Vec<Option<usize>>,
    pub(super) records: Vec<LaneRecord>,
    // Resident append bodies are the only records that a clear can retire.
    // Cold-only replay uses this as an O(1) guard before considering records.
    pub(super) hot_append_count: usize,
    #[cfg(test)]
    pub(super) retire_record_checks: usize,
    pub(super) open_operations_by_lane: HashMap<String, BTreeMap<u64, OperationStartedRecord>>,
    pub(super) lanes: BTreeMap<String, Option<String>>,
    /// Sequence of the latest explicit create/move mutation for each lane.
    /// This is deliberately independent of the history sequence and ordinary
    /// entry appends, so ABA navigation cannot be mistaken for an append.
    pub(super) lane_generations: BTreeMap<String, u64>,
    pub(super) log: Vec<StoredLogItem>,
    pub(super) stats: SessionStats,
    pub(super) name: Option<String>,
    pub(super) labels: HashMap<String, String>,
}

impl<P: Clone> IndexedState<P> {
    pub(crate) fn new() -> Self {
        let mut lanes = BTreeMap::new();
        lanes.insert("main".to_string(), None);
        Self {
            sequence: 0,
            used_ids: std::collections::HashSet::new(),
            entries: Vec::new(),
            metadata: Vec::new(),
            entries_by_id: HashMap::new(),
            nearest_user: Vec::new(),
            records: Vec::new(),
            hot_append_count: 0,
            #[cfg(test)]
            retire_record_checks: 0,
            open_operations_by_lane: HashMap::new(),
            lane_generations: lanes.keys().map(|lane| (lane.clone(), 0)).collect(),
            lanes,
            log: Vec::new(),
            stats: SessionStats::default(),
            name: None,
            labels: HashMap::new(),
        }
    }

    pub(crate) fn completed_progress_runs(
        &self,
    ) -> crate::session::progress::CompletedProgressRuns {
        let mut completed = crate::session::progress::CompletedProgressRuns::new();
        for record in &self.records {
            if let LaneRecord::OperationFinished(finished) = record {
                completed
                    .entry(finished.base.lane.clone())
                    .or_default()
                    .insert(finished.run_id.clone());
            }
        }
        completed
    }

    pub(crate) fn next_sequence(&self) -> u64 {
        self.sequence + 1
    }

    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) fn reserve_cold_frame(
        &mut self,
        frame: &crate::session::types::AssistantFrameRecord,
    ) -> SessionResult<()> {
        self.reserve_cold_frame_identity(&frame.base.id, frame.base.seq, &frame.base.lane)
    }

    pub(crate) fn reserve_cold_frame_identity(
        &mut self,
        id: &str,
        seq: u64,
        lane: &str,
    ) -> SessionResult<()> {
        if seq != self.sequence + 1 {
            return Err(SessionError::invalid_entry(
                "non-consecutive cold frame sequence",
            ));
        }
        if !self.lanes.contains_key(lane) {
            return Err(SessionError::invalid_entry(format!(
                "Invalid session mutation: references missing lane {lane}"
            )));
        }
        self.validate_unused_id(id)?;
        self.sequence = seq;
        self.used_ids.insert(id.to_owned());
        Ok(())
    }

    pub(crate) fn validate_queue_cancellations(&self, records: &[LaneRecord]) -> SessionResult<()> {
        let mut ids = std::collections::HashSet::new();
        let mut targets = std::collections::HashSet::new();
        for record in records {
            let LaneRecord::QueueCancelled(cancel) = record else {
                return Err(SessionError::storage(
                    "cancellation batch contains a non-cancellation",
                ));
            };
            self.require_lane(&cancel.base.lane)?;
            self.validate_unused_id(&cancel.base.id)?;
            if !ids.insert(cancel.base.id.as_str()) || !targets.insert(cancel.entry_id.as_str()) {
                return Err(SessionError::storage("duplicate queue cancellation"));
            }
            let enqueue = self.find_records(&RecordQuery {
                lane: Some(cancel.base.lane.clone()),
                target_entry_id: Some(cancel.entry_id.clone()),
                pending_queue_only: true,
                limit: Some(1),
                ..Default::default()
            })?;
            if !matches!(enqueue.first(), Some(LaneRecord::QueueEnqueued(enqueued)) if enqueued.run_id == cancel.run_id)
            {
                return Err(SessionError::storage(
                    "queue cancellation has no pending matching enqueue",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn check_revision(&self, expected: u64) -> SessionResult<()> {
        if self.sequence != expected {
            return Err(SessionError::storage(
                "branch write conflict: history changed during conditional write",
            ));
        }
        Ok(())
    }

    pub(crate) fn resync_sequence(&mut self, seq: u64) {
        if seq > self.sequence {
            self.sequence = seq;
        }
    }

    pub(crate) fn lane_branch(&self, lane: &str) -> SessionResult<(Option<String>, u64)> {
        let leaf = self.require_lane(lane)?;
        let generation = self
            .lane_generations
            .get(lane)
            .copied()
            .ok_or_else(|| SessionError::storage("lane branch generation is unavailable"))?;
        Ok((leaf, generation))
    }

    pub(crate) fn get_lanes(&self) -> Vec<LanePointer> {
        self.lanes
            .iter()
            .map(|(lane, leaf_id)| LanePointer {
                lane: lane.clone(),
                leaf_id: leaf_id.clone(),
            })
            .collect()
    }

    pub(crate) fn require_lane(&self, lane: &str) -> SessionResult<Option<String>> {
        self.lanes
            .get(lane)
            .cloned()
            .ok_or_else(|| SessionError::invalid_lane(format!("Lane not found: {lane}")))
    }

    pub(crate) fn validate_new_lane(&self, lane: &str) -> SessionResult<()> {
        if self.lanes.contains_key(lane) {
            return Err(SessionError::already_exists(format!(
                "Lane already exists: {lane}"
            )));
        }
        Ok(())
    }

    pub(crate) fn validate_target(&self, target_id: Option<&str>) -> SessionResult<()> {
        if let Some(id) = target_id {
            if !self.entries_by_id.contains_key(id) {
                return Err(SessionError::not_found(format!("Entry not found: {id}")));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_unused_id(&self, id: &str) -> SessionResult<()> {
        if self.used_ids.contains(id) {
            return Err(SessionError::already_exists(format!(
                "Session id already exists: {id}"
            )));
        }
        Ok(())
    }

    pub(crate) fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        validate_limit(query.limit)?;
        let order = query.order.unwrap_or_default();
        let mut results = Vec::new();
        // Borrow cancellation identities, not historical message payloads.
        let cancelled: std::collections::HashSet<(&str, &str)> = if query.pending_queue_only {
            self.records
                .iter()
                .filter_map(|record| match record {
                    LaneRecord::QueueCancelled(cancel) => {
                        Some((cancel.base.lane.as_str(), cancel.entry_id.as_str()))
                    }
                    _ => None,
                })
                .collect()
        } else {
            Default::default()
        };
        let iter: Box<dyn Iterator<Item = &LaneRecord>> = match order {
            EntryOrder::OldestFirst => Box::new(self.records.iter()),
            EntryOrder::NewestFirst => Box::new(self.records.iter().rev()),
        };
        for record in iter {
            if !self.matches_record_query(record, query) {
                continue;
            }
            if query.pending_queue_only {
                let LaneRecord::QueueEnqueued(enqueued) = record else {
                    continue;
                };
                let Some(id) = enqueued
                    .target
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                if self.entries_by_id.contains_key(id) || cancelled.contains(&(record.lane(), id)) {
                    continue;
                }
            }
            results.push(record.clone());
            if let Some(limit) = query.limit {
                if results.len() >= limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub(crate) fn record_page(
        &self,
        query: &crate::session::types::RecordPageQuery,
    ) -> SessionResult<crate::session::types::RecordPage> {
        validate_limit(Some(query.limit))?;
        let matches = |record: &&LaneRecord| {
            record.lane() == query.lane
                && query.after_seq.is_none_or(|seq| record.seq() > seq)
                && query.before_seq.is_none_or(|seq| record.seq() < seq)
        };
        let total = self.records.iter().filter(matches).count();
        let mut records: Vec<_> = if query.from_end {
            self.records
                .iter()
                .rev()
                .filter(matches)
                .take(query.limit)
                .cloned()
                .collect()
        } else {
            self.records
                .iter()
                .filter(matches)
                .take(query.limit)
                .cloned()
                .collect()
        };
        if query.from_end {
            records.reverse();
        }
        Ok(crate::session::types::RecordPage { records, total })
    }

    pub(crate) fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        validate_limit(limit)?;
        let mut open: Vec<OperationStartedRecord> = self
            .open_operations_by_lane
            .get(lane)
            .map(|m| m.values().rev().cloned().collect())
            .unwrap_or_default();
        if let Some(limit) = limit {
            open.truncate(limit);
        }
        Ok(open)
    }

    pub(crate) fn has_open_operation_except(
        &self,
        lane: &str,
        except_run_id: Option<&str>,
    ) -> bool {
        self.open_operations_by_lane
            .get(lane)
            .map(|open| {
                open.values()
                    .any(|started| Some(started.base.id.as_str()) != except_run_id)
            })
            .unwrap_or(false)
    }

    pub(crate) fn get_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub(crate) fn get_label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(|s| s.as_str())
    }

    pub(crate) fn get_stats(&self) -> &SessionStats {
        &self.stats
    }

    pub(crate) fn matches_record_query(&self, record: &LaneRecord, query: &RecordQuery) -> bool {
        let lane_ok = query
            .lane
            .as_deref()
            .map(|l| record.lane() == l)
            .unwrap_or(true);
        let type_ok = query
            .record_type
            .map(|t| t == record.record_type())
            .unwrap_or(true);
        let run_ok = query
            .run_id
            .as_deref()
            .map(|run| match record {
                LaneRecord::OperationStarted(s) => s.base.id == run,
                _ => record.run_id().map(|r| r == run).unwrap_or(false),
            })
            .unwrap_or(true);
        let kind_ok = query
            .operation_kind
            .map(|k| {
                matches!(record,
                    LaneRecord::OperationStarted(s) if s.intent.kind() == k)
            })
            .unwrap_or(true);
        let after_ok = query
            .after_seq
            .map(|after| record.seq() > after)
            .unwrap_or(true);
        let queue_ok = query.queue_kind.is_none_or(
            |kind| matches!(record, LaneRecord::QueueEnqueued(enqueued) if enqueued.queue == kind),
        );
        let target_ok = query.target_entry_id.as_deref().is_none_or(|id| {
            matches!(record, LaneRecord::QueueEnqueued(enqueued) if enqueued.target.get("id").and_then(serde_json::Value::as_str) == Some(id))
        });
        let tool_ok = query.tool_call_id.as_deref().is_none_or(
            |id| matches!(record, LaneRecord::ToolStarted(started) if started.tool_call_id == id),
        );
        lane_ok && type_ok && run_ok && kind_ok && after_ok && queue_ok && target_ok && tool_ok
    }

    pub(crate) fn apply_mutation(
        &mut self,
        mutation: SessionMutation,
        mut payload: impl FnMut(&Entry) -> P,
    ) -> SessionResult<ApplyOutcome> {
        let seq = mutation.seq();
        if seq != self.sequence + 1 {
            return Err(SessionError::invalid_entry(format!(
                "Invalid session mutation: has non-consecutive seq {seq}"
            )));
        }

        match mutation {
            SessionMutation::Entry {
                seq, lane, entry, ..
            } => {
                self.validate_unused_id(entry.id())?;
                // Lane chaining — only when a lane is named (fork passes None).
                if let Some(lane_name) = lane.as_deref() {
                    let leaf = self.lanes.get(lane_name).cloned().ok_or_else(|| {
                        SessionError::invalid_entry(format!(
                            "Invalid session mutation: references missing lane {lane_name}"
                        ))
                    })?;
                    // `entry.parent_id` must equal the lane's current leaf.
                    if entry.parent_id().map(|s| s.to_string()) != leaf {
                        return Err(SessionError::invalid_entry(
                            "Invalid session mutation: does not chain to the lane leaf".to_string(),
                        ));
                    }
                }
                // Parent existence (when set).
                if let Some(parent) = entry.parent_id() {
                    if !self.entries_by_id.contains_key(parent) {
                        return Err(SessionError::invalid_entry(format!(
                            "Invalid session mutation: references missing parent {parent}"
                        )));
                    }
                }

                let meta = EntryMeta::from_entry(&entry);
                // Construct payload only after validation, before committing any state.
                let payload = payload(&entry);
                let nearest_user = if meta.is_user() {
                    Some(self.entries.len())
                } else {
                    entry
                        .parent_id()
                        .and_then(|id| self.entries_by_id.get(id))
                        .and_then(|index| self.nearest_user[*index])
                };
                self.nearest_user.push(nearest_user);
                let is_message = matches!(entry, Entry::Message(_));
                self.sequence = seq;
                self.used_ids.insert(entry.id().to_string());
                self.entries_by_id
                    .insert(entry.id().to_string(), self.entries.len());
                if let Some(lane_name) = lane.as_deref() {
                    if let Some(slot) = self.lanes.get_mut(lane_name) {
                        *slot = Some(entry.id().to_string());
                    }
                }
                self.log.push(StoredLogItem::Entry(self.entries.len()));
                self.metadata.push(meta);
                self.entries.push(payload);
                if is_message {
                    self.stats.message_count += 1;
                }
                Ok(ApplyOutcome::Entry(entry))
            }
            SessionMutation::Record { record } => {
                let lane = record.lane().to_string();
                if !self.lanes.contains_key(&lane) {
                    return Err(SessionError::invalid_entry(format!(
                        "Invalid session mutation: references missing lane {lane}"
                    )));
                }
                self.validate_unused_id(record.id())?;
                let seq = record.seq();
                self.sequence = seq;
                self.used_ids.insert(record.id().to_string());
                match &record {
                    LaneRecord::OperationStarted(started) => {
                        self.open_operations_by_lane
                            .entry(lane.clone())
                            .or_default()
                            .insert(seq, started.clone());
                    }
                    LaneRecord::OperationFinished(finished) => {
                        if let Some(open) = self.open_operations_by_lane.get_mut(&lane) {
                            // Remove by run_id (matching the operation_started id).
                            open.retain(|_, started| started.base.id != finished.run_id);
                            if open.is_empty() {
                                self.open_operations_by_lane.remove(&lane);
                            }
                        }
                    }
                    _ => {}
                }
                if let LaneRecord::Usage(usage_rec) = &record {
                    self.stats.cached_tokens += usage_rec.usage.cache_read;
                    self.stats.uncached_tokens +=
                        usage_rec.usage.input + usage_rec.usage.cache_write;
                    self.stats.total_tokens += usage_rec.usage.total_tokens;
                    self.stats.cost_total += usage_rec.usage.cost.total;
                }
                self.log.push(StoredLogItem::Record(self.records.len()));
                self.records.push(record.clone());
                if let LaneRecord::AssistantFrame(frame) = &record {
                    if frame.op == AssistantFrameOp::Append {
                        self.hot_append_count += 1;
                    }
                    self.retire_cleared_frames(frame);
                }
                Ok(ApplyOutcome::Record(record))
            }
            SessionMutation::Lane { seq, lane, leaf_id } => {
                if let Some(leaf) = leaf_id.as_deref() {
                    if !self.entries_by_id.contains_key(leaf) {
                        return Err(SessionError::invalid_entry(format!(
                            "Invalid session mutation: references missing lane target {leaf}"
                        )));
                    }
                }
                self.sequence = seq;
                self.lanes.insert(lane.clone(), leaf_id.clone());
                self.lane_generations.insert(lane.clone(), seq);
                self.log.push(StoredLogItem::Other(Box::new(LogItem::Lane {
                    seq,
                    lane,
                    leaf_id,
                })));
                Ok(ApplyOutcome::Lane)
            }
            SessionMutation::FactName { seq, name } => {
                self.sequence = seq;
                self.name = name.clone();
                self.log
                    .push(StoredLogItem::Other(Box::new(LogItem::FactName {
                        seq,
                        name,
                    })));
                Ok(ApplyOutcome::Fact)
            }
            SessionMutation::FactLabel {
                seq,
                target_id,
                label,
            } => {
                if !self.entries_by_id.contains_key(&target_id) {
                    return Err(SessionError::invalid_entry(format!(
                        "Invalid session mutation: references missing label target {target_id}"
                    )));
                }
                self.sequence = seq;
                match &label {
                    Some(l) => {
                        self.labels.insert(target_id.clone(), l.clone());
                    }
                    None => {
                        self.labels.remove(&target_id);
                    }
                }
                self.log
                    .push(StoredLogItem::Other(Box::new(LogItem::FactLabel {
                        seq,
                        target_id,
                        label,
                    })));
                Ok(ApplyOutcome::Fact)
            }
        }
    }
}
