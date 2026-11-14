//! JSONL-private payload references. Query plans own only selected sources; no
//! warm cache and no published plan holds or clones the resident history index.
use super::{codec::parse_mutation, errors::file_result};
use crate::{
    error::{SessionError, SessionResult},
    session::{
        state::{matches_entry_payload, ApplyOutcome, IndexedState, StoredLogItem},
        types::*,
    },
};
use rpi_tools::env::{AppendReceipt, ReadFile};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
};

/// A single JSONL row may exceed the scanner's 1 MiB I/O window. Keep
/// hydration bounded at the reader boundary too, without splitting JSON values
/// or weakening the per-location digest and identity checks.
async fn read_range_bounded(
    reader: &Arc<dyn ReadFile>,
    offset: u64,
    length: usize,
) -> SessionResult<Vec<u8>> {
    const CHUNK: usize = 1024 * 1024;
    if length <= CHUNK {
        return file_result(
            reader.read_range(offset, length).await,
            "read cold history range",
        );
    }
    let mut bytes = Vec::with_capacity(length);
    for start in (0..length).step_by(CHUNK) {
        let count = (length - start).min(CHUNK);
        let at = offset
            .checked_add(start as u64)
            .ok_or_else(|| SessionError::storage("cold history range overflow"))?;
        let chunk = file_result(
            reader.read_range(at, count).await,
            "read cold history range",
        )?;
        if chunk.len() != count {
            return Err(SessionError::storage("short cold history read"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Clone)]
pub(super) struct Location {
    reader: Arc<dyn ReadFile>,
    seq: u64,
    stream_index: usize,
    append: bool,
    retired: bool,
    offset: u64,
    length: usize,
    digest: [u8; 32],
    id: String,
    lane: Arc<str>,
    run: Arc<str>,
}
impl Location {
    fn new(reader: Arc<dyn ReadFile>, offset: u64, raw: &[u8], mutation: &SessionMutation) -> Self {
        let (id, lane, run): (String, String, String) = match mutation {
            SessionMutation::Entry { entry, .. } => {
                (entry.id().into(), String::new(), String::new())
            }
            SessionMutation::Record { record } => (
                record.id().into(),
                record.lane().into(),
                record.run_id().unwrap_or_default().into(),
            ),
            _ => (String::new(), String::new(), String::new()),
        };
        let (stream_index, append) = match mutation {
            SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame),
            } => (frame.stream_index, frame.op == AssistantFrameOp::Append),
            _ => (0, false),
        };
        Self {
            reader,
            seq: mutation.seq(),
            stream_index,
            append,
            retired: false,
            offset,
            length: raw.len(),
            digest: Sha256::digest(raw).into(),
            id,
            lane: Arc::from(lane),
            run: Arc::from(run),
        }
    }
    async fn read(&self) -> SessionResult<SessionMutation> {
        let raw = read_range_bounded(&self.reader, self.offset, self.length).await?;
        self.decode(&raw)
    }
    fn decode(&self, raw: &[u8]) -> SessionResult<SessionMutation> {
        if raw.len() != self.length {
            return Err(SessionError::storage("short cold history read"));
        }
        if <[u8; 32]>::from(Sha256::digest(raw)) != self.digest {
            return Err(SessionError::invalid_entry(
                "cold history source changed; reopen session",
            ));
        }
        let line = std::str::from_utf8(raw)
            .map_err(|_| SessionError::invalid_entry("invalid cold history UTF-8"))?;
        let mutation = parse_mutation(line)
            .map_err(|e| SessionError::invalid_entry(format!("invalid cold history: {e}")))?;
        let identity = match &mutation {
            SessionMutation::Entry { entry, .. } => entry.id() == self.id,
            SessionMutation::Record { record } => {
                record.id() == self.id
                    && record.lane() == self.lane.as_ref()
                    && record.run_id().unwrap_or_default() == self.run.as_ref()
            }
            _ => false,
        };
        if mutation.seq() != self.seq || !identity {
            return Err(SessionError::invalid_entry(
                "cold history identity mismatch",
            ));
        }
        Ok(mutation)
    }
}
/// Live entries can distinguish `Some(Null)` from `None`, whereas the legacy
/// message decoder intentionally cannot. Keep only typed presence corrections,
/// never message bodies or raw JSON, until this live generation is dropped.
#[derive(Clone, Copy)]
enum MessageNull {
    ToolDetails,
    DeferredData,
}
impl MessageNull {
    fn capture(message: &rpi_agent::message::AgentMessage) -> Option<Self> {
        use rpi_agent::message::AgentMessage;
        match message {
            AgentMessage::ToolResult(message)
                if message.details.as_ref().is_some_and(|v| v.is_null()) =>
            {
                Some(Self::ToolDetails)
            }
            AgentMessage::Assistant(message)
                if message
                    .deferred
                    .as_ref()
                    .and_then(|d| d.data.as_ref())
                    .is_some_and(|v| v.is_null()) =>
            {
                Some(Self::DeferredData)
            }
            _ => None,
        }
    }
    fn apply(self, message: &mut rpi_agent::message::AgentMessage) {
        use rpi_agent::message::AgentMessage;
        match (self, message) {
            (Self::ToolDetails, AgentMessage::ToolResult(message)) => {
                message.details = Some(serde_json::Value::Null);
            }
            (Self::DeferredData, AgentMessage::Assistant(message)) => {
                if let Some(deferred) = &mut message.deferred {
                    deferred.data = Some(serde_json::Value::Null);
                }
            }
            _ => {}
        }
    }
}
#[derive(Clone, Default)]
pub(super) struct LiveNulls {
    message: Option<MessageNull>,
    retained_tail: Vec<(usize, MessageNull)>,
}
impl LiveNulls {
    fn capture(entry: &Entry) -> Self {
        match entry {
            Entry::Message(entry) => Self {
                message: MessageNull::capture(&entry.message),
                ..Self::default()
            },
            Entry::Compaction(entry) => Self {
                retained_tail: entry
                    .retained_tail
                    .iter()
                    .enumerate()
                    .filter_map(|(i, message)| {
                        MessageNull::capture(message).map(|correction| (i, correction))
                    })
                    .collect(),
                ..Self::default()
            },
            _ => Self::default(),
        }
    }
    fn apply(&self, entry: &mut Entry) {
        match entry {
            Entry::Message(entry) => {
                if let Some(correction) = self.message {
                    correction.apply(&mut entry.message);
                }
            }
            Entry::Compaction(entry) => {
                for &(index, correction) in &self.retained_tail {
                    if let Some(message) = entry.retained_tail.get_mut(index) {
                        correction.apply(message);
                    }
                }
            }
            _ => {}
        }
    }
}
#[derive(Clone)]
pub(super) enum JsonlPayload {
    Resident(Arc<Entry>),
    Cold {
        source: Arc<Location>,
        parent: Option<String>,
        live_nulls: LiveNulls,
    },
}
impl JsonlPayload {
    pub async fn read(&self) -> SessionResult<Entry> {
        match self {
            Self::Resident(entry) => Ok((**entry).clone()),
            Self::Cold {
                source,
                parent,
                live_nulls,
            } => restore_entry(source.read().await?, parent, live_nulls),
        }
    }
}
#[derive(Clone, Default)]
struct ColdFrames {
    locations: Vec<Arc<Location>>,
    // Only unretired append locations. Clears do not repeatedly walk all prior
    // runs; immutable read plans continue owning their original Location Arcs.
    active: HashMap<String, HashMap<usize, Vec<usize>>>,
}
#[derive(Clone)]
pub(super) struct ResidentState {
    reducer: IndexedState<JsonlPayload>,
    cold: ColdFrames,
    // Only the currently durable batch, consumed by apply_mutation. Custom
    // appenders without receipts simply leave real payloads resident.
    pending: HashMap<u64, Arc<Location>>,
    // Most consecutive historical frames share one lane and run. Keep at most
    // these two symbols, not a lifetime-wide interning table.
    last_frame_lane: Option<Arc<str>>,
    last_frame_run: Option<Arc<str>>,
}
impl Default for ResidentState {
    fn default() -> Self {
        Self {
            reducer: IndexedState::new(),
            cold: ColdFrames::default(),
            pending: HashMap::new(),
            last_frame_lane: None,
            last_frame_run: None,
        }
    }
}
impl Deref for ResidentState {
    type Target = IndexedState<JsonlPayload>;
    fn deref(&self) -> &Self::Target {
        &self.reducer
    }
}
impl DerefMut for ResidentState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.reducer
    }
}
impl ResidentState {
    #[cfg(test)]
    pub(super) fn assert_cold_shared(&self, ids: &[String]) -> Arc<dyn ReadFile> {
        assert!(
            self.pending.is_empty(),
            "acknowledged appends must consume pending locations"
        );
        let mut reader = None;
        for id in ids {
            let Some(JsonlPayload::Cold { source, .. }) = self.entry_ref(id) else {
                panic!("entry {id} must retain only a cold payload");
            };
            if let Some(first) = &reader {
                assert!(
                    Arc::ptr_eq(first, &source.reader),
                    "one generation must share one reader Arc"
                );
            } else {
                reader = Some(source.reader.clone());
            }
        }
        reader.expect("nonempty entry ids")
    }

    pub fn apply_mutation(&mut self, mutation: SessionMutation) -> SessionResult<ApplyOutcome> {
        let source = self.pending.remove(&mutation.seq());
        self.apply_source(mutation, source)
    }
    fn apply_source(
        &mut self,
        mutation: SessionMutation,
        source: Option<Arc<Location>>,
    ) -> SessionResult<ApplyOutcome> {
        if let (
            Some(source),
            SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame),
            },
        ) = (&source, &mutation)
        {
            if frame.op != AssistantFrameOp::CommitStream {
                self.reducer.reserve_cold_frame(frame)?;
                self.reducer.retire_cleared_frames(frame);
                self.cold.retire(frame);
                self.cold.push(source.clone());
                return Ok(ApplyOutcome::Record(LaneRecord::AssistantFrame(
                    frame.clone(),
                )));
            }
        }
        let outcome = self
            .reducer
            .apply_mutation(mutation, |entry| match &source {
                Some(source) => JsonlPayload::Cold {
                    source: source.clone(),
                    parent: entry.parent_id().map(str::to_owned),
                    live_nulls: LiveNulls::capture(entry),
                },
                None => JsonlPayload::Resident(Arc::new(entry.clone())),
            })?;
        if let ApplyOutcome::Record(LaneRecord::AssistantFrame(frame)) = &outcome {
            self.cold.retire(frame);
        }
        Ok(outcome)
    }
    /// Detach one selected row at a time; retired frame bodies are never read.
    pub async fn into_detached(mut self) -> SessionResult<crate::session::state::SessionState> {
        let mut entries = Vec::new();
        for payload in self.reducer.take_payloads() {
            entries.push(payload.read().await?);
        }
        let mut frames = Vec::new();
        for location in self.cold.locations {
            if !location.retired {
                let SessionMutation::Record { record } = location.read().await? else {
                    return Err(SessionError::invalid_entry("cold frame identity mismatch"));
                };
                frames.push(record);
            }
        }
        Ok(self.reducer.into_session_state(entries, frames))
    }
    /// Facts-only preflight still validates every historical ID/sequence/lane,
    /// but exports no frame-query capability and needs no payload locations.
    pub fn reserve_loaded_frame(
        &mut self,
        metadata: super::cold_decode::ColdFrame<'_>,
    ) -> SessionResult<()> {
        self.reserve_frame_metadata(&metadata)
    }
    fn reserve_frame_metadata(
        &mut self,
        metadata: &super::cold_decode::ColdFrame<'_>,
    ) -> SessionResult<()> {
        self.reducer
            .reserve_cold_frame_identity(&metadata.id, metadata.seq, &metadata.lane)?;
        if matches!(
            metadata.op,
            AssistantFrameOp::ClearStream | AssistantFrameOp::ClearRun
        ) {
            let frame = AssistantFrameRecord {
                base: RecordBase {
                    id: metadata.id.to_string(),
                    seq: metadata.seq,
                    timestamp: metadata.timestamp,
                    lane: metadata.lane.to_string(),
                },
                run_id: metadata.run_id.to_string(),
                stream_index: metadata.stream_index,
                op: metadata.op,
                frame: None,
            };
            self.reducer.retire_cleared_frames(&frame);
            self.cold.retire(&frame);
        }
        Ok(())
    }
    /// Apply a validated historical frame without constructing its discarded
    /// JSON payload or a payload-bearing ApplyOutcome. Hydration still uses the
    /// original codec and the same digest/reader-generation checks.
    pub fn apply_loaded_frame(
        &mut self,
        metadata: super::cold_decode::ColdFrame<'_>,
        reader: &Arc<dyn ReadFile>,
        offset: u64,
        raw: &[u8],
    ) -> SessionResult<()> {
        self.reserve_frame_metadata(&metadata)?;
        fn symbol(slot: &mut Option<Arc<str>>, value: &str) -> Arc<str> {
            if let Some(current) = slot.as_ref().filter(|current| current.as_ref() == value) {
                return current.clone();
            }
            let current = Arc::<str>::from(value);
            *slot = Some(current.clone());
            current
        }
        let lane = symbol(&mut self.last_frame_lane, &metadata.lane);
        let run = symbol(&mut self.last_frame_run, &metadata.run_id);
        self.cold.push(Arc::new(Location {
            reader: reader.clone(),
            seq: metadata.seq,
            stream_index: metadata.stream_index,
            append: metadata.op == AssistantFrameOp::Append,
            retired: false,
            offset,
            length: raw.len(),
            digest: Sha256::digest(raw).into(),
            id: metadata.id.into_owned(),
            lane,
            run,
        }));
        Ok(())
    }
    pub fn apply_loaded(
        &mut self,
        mutation: SessionMutation,
        reader: &Arc<dyn ReadFile>,
        offset: u64,
        raw: &[u8],
    ) -> SessionResult<()> {
        let source = matches!(
            mutation,
            SessionMutation::Entry { .. } | SessionMutation::Record { .. }
        )
        .then(|| Arc::new(Location::new(reader.clone(), offset, raw, &mutation)));
        self.apply_source(mutation, source).map(|_| ())
    }
    pub fn bind_append(&mut self, receipt: AppendReceipt, bytes: &[u8]) {
        let mut offset = receipt.offset;
        for raw in bytes.split_inclusive(|byte| *byte == b'\n') {
            if let Ok(line) = std::str::from_utf8(raw) {
                if let Ok(mutation) = parse_mutation(line) {
                    if matches!(
                        mutation,
                        SessionMutation::Entry { .. } | SessionMutation::Record { .. }
                    ) {
                        let source = Arc::new(Location::new(
                            receipt.reader.clone(),
                            offset,
                            raw,
                            &mutation,
                        ));
                        // Candidate batches have already applied entries; replace
                        // only their payload slots, never replay their mutations.
                        if let SessionMutation::Entry { entry, .. } = &mutation {
                            if let Some(index) = self.reducer.entry_index(entry.id()) {
                                let parent =
                                    self.reducer.entry_meta(index).unwrap().parent_id.clone();
                                // The decoded mutation has already lost message
                                // Some(Null). Preserve the actual candidate's
                                // live semantics instead of inspecting that row.
                                let live_nulls = match self.reducer.payload(index).unwrap() {
                                    JsonlPayload::Resident(entry) => LiveNulls::capture(entry),
                                    JsonlPayload::Cold { live_nulls, .. } => live_nulls.clone(),
                                };
                                *self.reducer.payload_mut(index).unwrap() = JsonlPayload::Cold {
                                    source,
                                    parent,
                                    live_nulls,
                                };
                                offset += raw.len() as u64;
                                continue;
                            }
                        }
                        self.pending.insert(mutation.seq(), source);
                    }
                }
            }
            offset += raw.len() as u64;
        }
    }
    #[cfg(test)]
    pub fn defer(
        &mut self,
        frame: &AssistantFrameRecord,
        reader: &Arc<dyn ReadFile>,
        offset: u64,
        line: &str,
    ) -> SessionResult<()> {
        self.apply_loaded(
            SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame.clone()),
            },
            reader,
            offset,
            line.as_bytes(),
        )
    }
    pub fn entry_ref(&self, id: &str) -> Option<JsonlPayload> {
        self.reducer
            .entry_index(id)
            .and_then(|i| self.reducer.payload(i))
            .cloned()
    }
    fn refs(&self, indices: impl IntoIterator<Item = usize>) -> Vec<JsonlPayload> {
        indices
            .into_iter()
            .map(|i| self.reducer.payload(i).unwrap().clone())
            .collect()
    }
    pub fn entry_plan(
        &self,
        query: &EntryQuery,
        branch: Option<(&BranchBounds, &str)>,
    ) -> SessionResult<EntryPlan> {
        let mut indices = self.reducer.select_entry_indices(query, branch)?;
        if query.tool_call_id.is_none() && query.outbox_key.is_none() {
            indices.truncate(query.limit.unwrap_or(usize::MAX));
        }
        Ok(EntryPlan {
            selected: self.refs(indices),
            query: query.clone(),
        })
    }
    pub fn branch_ancestry(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<EntryAncestry>> {
        self.reducer.branch_ancestry(bounds, start)
    }
    pub fn context_plan(&self, bounds: &BranchBounds, start: &str) -> SessionResult<ContextPlan> {
        let query = EntryQuery {
            // Selection still validates the full ancestry and preserves all
            // non-archival metadata facts and compaction boundaries.
            exclude_tool_program_bodies: true,
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        };
        let path = self
            .reducer
            .select_entry_indices(&query, Some((bounds, start)))?;
        let mut facts = crate::session::context::SessionContext::default();
        facts.thinking_level = "off".into();
        let mut thinking = false;
        for &i in path.iter().rev() {
            let meta = self.reducer.entry_meta(i).unwrap();
            if !thinking {
                if let Some(level) = &meta.thinking_level {
                    facts.thinking_level = level.clone();
                    thinking = true;
                }
            }
            if facts.model.is_none() {
                facts.model = meta.model_change.clone().or_else(|| {
                    Some((
                        meta.assistant_provider.clone()?,
                        meta.assistant_model.clone()?,
                    ))
                });
            }
            if facts.active_tool_names.is_none() {
                facts.active_tool_names = meta.active_tools.clone();
            }
        }
        let first = path
            .iter()
            .rposition(|&i| self.reducer.entry_meta(i).unwrap().kind == "compaction")
            .unwrap_or(0);
        Ok(ContextPlan {
            selected: self.refs(path.into_iter().skip(first)),
            facts,
        })
    }
    /// Pin the lane leaf and payload generations together under the caller's
    /// state lock. Only default model-visible rows after the latest compaction
    /// can contribute; an ordinary assistant usage lets us skip earlier bodies.
    pub fn context_usage_plan(&self, lane: &str) -> SessionResult<ContextUsagePlan> {
        use rpi_ai::types::StopReason;
        let leaf = self.reducer.require_lane(lane)?;
        let mut path = self
            .reducer
            .walk_to_root(leaf.as_deref(), &BranchBounds::default())?;
        path.reverse();
        let boundary = path
            .iter()
            .rposition(|&i| self.reducer.entry_meta(i).unwrap().kind == "compaction")
            .unwrap_or(0);
        let path = &path[boundary..];
        let anchor = path
            .iter()
            .rposition(|&i| {
                let meta = self.reducer.entry_meta(i).unwrap();
                meta.kind == "message"
                    && meta.stop_reason.as_ref().is_some_and(|reason| {
                        !matches!(
                            reason,
                            StopReason::Deferred | StopReason::Aborted | StopReason::Error
                        )
                    })
                    && meta
                        .usage
                        .as_ref()
                        .is_some_and(|usage| usage.context_tokens() > 0)
            })
            .unwrap_or(0);
        let message_offset = path[..anchor]
            .iter()
            .map(|&i| self.reducer.entry_meta(i).unwrap().context_message_count)
            .sum();
        let selected = self.refs(
            path[anchor..]
                .iter()
                .copied()
                .filter(|&i| self.reducer.entry_meta(i).unwrap().context_message_count > 0),
        );
        Ok(ContextUsagePlan {
            selected,
            message_offset,
        })
    }

    pub fn transcript_plan(
        &self,
        lane: &str,
        query: &TranscriptPageQuery,
    ) -> SessionResult<TranscriptPlan> {
        let leaf_id = self.reducer.require_lane(lane)?;
        let mut indices = self
            .reducer
            .walk_to_root(leaf_id.as_deref(), &BranchBounds::default())?;
        indices.reverse();
        indices.retain(|&i| self.reducer.entry_meta(i).unwrap().is_transcript());
        let total = indices.len();
        let end = match &query.before_id {
            Some(id) => indices
                .iter()
                .position(|&i| self.reducer.entry_meta(i).unwrap().id == *id)
                .ok_or_else(|| SessionError::invalid_query(format!("beforeId not found: {id}")))?,
            None => total,
        };
        let start = end.saturating_sub(query.limit.max(1));
        Ok(TranscriptPlan {
            selected: self.refs(indices[start..end].iter().copied()),
            total,
            has_more: start > 0,
            leaf_id,
        })
    }
    pub fn summary_plan(
        &self,
        lane: &str,
        query: &MessageSummaryQuery,
    ) -> SessionResult<SummaryPlan> {
        let selected = self.reducer.select_message_summaries(lane, query)?;
        Ok(SummaryPlan {
            selected: self.refs(selected.indices),
            has_more: selected.has_more,
            leaf_id: selected
                .leaf
                .map(|i| self.reducer.entry_meta(i).unwrap().id.clone()),
        })
    }
    pub fn window_plan(&self, lane: &str, query: &MessageWindowQuery) -> SessionResult<WindowPlan> {
        let selected = self.reducer.select_message_window(lane, query)?;
        Ok(WindowPlan {
            selected: self.refs(selected.indices),
            before: selected.has_more_before,
            after: selected.has_more_after,
            leaf_id: selected
                .leaf
                .map(|i| self.reducer.entry_meta(i).unwrap().id.clone()),
        })
    }
    pub fn record_plan(&self, query: &RecordQuery) -> SessionResult<RecordPlan> {
        let hot = self.reducer.find_records(query)?;
        let mut selected: Vec<_> = hot.into_iter().map(Selected::Hot).collect();
        if !query
            .record_type
            .is_some_and(|kind| kind != "assistant_frame")
            && query.operation_kind.is_none()
            && query.queue_kind.is_none()
            && query.target_entry_id.is_none()
            && query.tool_call_id.is_none()
            && !query.pending_queue_only
        {
            let oldest = query.order == Some(EntryOrder::OldestFirst);
            let iter: Box<dyn Iterator<Item = &Arc<Location>> + '_> = if oldest {
                Box::new(self.cold.locations.iter())
            } else {
                Box::new(self.cold.locations.iter().rev())
            };
            selected.extend(
                iter.filter(|loc| {
                    !loc.retired
                        && query.lane.as_ref().is_none_or(|v| v == loc.lane.as_ref())
                        && query.run_id.as_ref().is_none_or(|v| v == loc.run.as_ref())
                        && query.after_seq.is_none_or(|v| loc.seq > v)
                })
                .take(query.limit.unwrap_or(usize::MAX))
                .cloned()
                .map(Selected::Cold),
            );
        }
        selected.sort_unstable_by_key(|item| item.seq(LaneRecord::seq));
        if query.order != Some(EntryOrder::OldestFirst) {
            selected.reverse();
        }
        selected.truncate(query.limit.unwrap_or(usize::MAX));
        Ok(RecordPlan { selected })
    }
    pub fn page_plan(&self, query: &RecordPageQuery) -> SessionResult<(RecordPlan, usize)> {
        let hot = self.reducer.record_page(query)?;
        let matching = |loc: &&Arc<Location>| {
            !loc.retired
                && loc.lane.as_ref() == query.lane
                && query.after_seq.is_none_or(|v| loc.seq > v)
                && query.before_seq.is_none_or(|v| loc.seq < v)
        };
        let total = hot.total + self.cold.locations.iter().filter(matching).count();
        let mut selected: Vec<_> = hot.records.into_iter().map(Selected::Hot).collect();
        let iter: Box<dyn Iterator<Item = &Arc<Location>> + '_> = if query.from_end {
            Box::new(self.cold.locations.iter().rev())
        } else {
            Box::new(self.cold.locations.iter())
        };
        selected.extend(
            iter.filter(matching)
                .take(query.limit)
                .cloned()
                .map(Selected::Cold),
        );
        selected.sort_unstable_by_key(|item| item.seq(LaneRecord::seq));
        if query.from_end {
            selected.drain(..selected.len().saturating_sub(query.limit));
        } else {
            selected.truncate(query.limit);
        }
        Ok((RecordPlan { selected }, total))
    }
    pub fn log_plan(&self, options: &LogOptions) -> SessionResult<LogPlan> {
        let mut selected = Vec::new();
        for item in self.reducer.log_items(options)? {
            selected.push(match item {
                StoredLogItem::Entry(i) => LogSelected::Entry(
                    self.reducer.entry_meta(*i).unwrap().seq,
                    self.reducer.payload(*i).unwrap().clone(),
                ),
                StoredLogItem::Record(i) => {
                    let record = self.reducer.records()[*i].clone();
                    LogSelected::Hot(LogItem::Record {
                        seq: record.seq(),
                        record,
                    })
                }
                StoredLogItem::Other(item) => LogSelected::Hot((**item).clone()),
            });
        }
        selected.extend(
            self.cold
                .locations
                .iter()
                .filter(|loc| !loc.retired && options.after_seq.is_none_or(|v| loc.seq > v))
                .take(options.limit.unwrap_or(usize::MAX))
                .cloned()
                .map(LogSelected::Cold),
        );
        selected.sort_unstable_by_key(LogSelected::seq);
        selected.truncate(options.limit.unwrap_or(usize::MAX));
        Ok(LogPlan { selected })
    }
    pub fn fork_plan(
        &self,
        options: &ForkOptions,
        include_values: bool,
    ) -> SessionResult<ForkPlan> {
        let (indices, lanes) = match options {
            ForkOptions::Tree => (
                (0..self.reducer.metadata().len()).collect::<Vec<_>>(),
                self.reducer.get_lanes(),
            ),
            ForkOptions::Branch { entry_id, position } => {
                let selected = entry_id.clone().or(self.reducer.require_lane("main")?);
                let target = if let Some(id) = selected {
                    let i = self
                        .reducer
                        .entry_index(&id)
                        .ok_or_else(|| SessionError::not_found(format!("Entry not found: {id}")))?;
                    // An omitted target denotes the actual leaf, including
                    // custom/model/thinking entries. Only explicit targets
                    // retain the ordinary message-only contract.
                    let meta = self.reducer.entry_meta(i).unwrap();
                    if entry_id.is_some() && meta.kind != "message" {
                        return Err(SessionError::invalid_fork_target(format!(
                            "Fork target is not a message entry: {id}"
                        )));
                    }
                    match position.unwrap_or(if entry_id.is_some() {
                        ForkPosition::Before
                    } else {
                        ForkPosition::At
                    }) {
                        ForkPosition::At => Some(meta.id.clone()),
                        ForkPosition::Before => meta.parent_id.clone(),
                    }
                } else {
                    None
                };
                let mut indices = self
                    .reducer
                    .walk_to_root(target.as_deref(), &BranchBounds::default())?;
                indices.reverse();
                (
                    indices,
                    vec![LanePointer {
                        lane: "main".into(),
                        leaf_id: target,
                    }],
                )
            }
        };
        let mut labels = Vec::new();
        for &i in &indices {
            let id = &self.reducer.entry_meta(i).unwrap().id;
            if let Some(label) = self.reducer.get_label(id) {
                labels.push((id.clone(), label.to_owned()));
            }
        }
        Ok(ForkPlan {
            entries: self.refs(indices),
            lanes,
            labels,
            name: self.reducer.get_name().map(str::to_owned),
            // Source-wide latest values, not merely values on the selected
            // branch. Capture references under the same lock/reader generation
            // as the cut; hydration and host validation happen before publish.
            source_values: if include_values {
                Some(self.entry_plan(
                    &EntryQuery {
                        entry_type: Some("custom"),
                        custom_type: Some(crate::session::values::VALUE_CUSTOM_TYPE.into()),
                        order: Some(EntryOrder::OldestFirst),
                        ..Default::default()
                    },
                    None,
                )?)
            } else {
                None
            },
        })
    }
}
pub(super) struct ForkPlan {
    pub entries: Vec<JsonlPayload>,
    pub lanes: Vec<LanePointer>,
    pub labels: Vec<(String, String)>,
    pub name: Option<String>,
    pub source_values: Option<EntryPlan>,
}
fn restore_entry(
    mutation: SessionMutation,
    parent: &Option<String>,
    live_nulls: &LiveNulls,
) -> SessionResult<Entry> {
    let SessionMutation::Entry { mut entry, .. } = mutation else {
        return Err(SessionError::invalid_entry("cold entry identity mismatch"));
    };
    entry.base_mut().parent_id = parent.clone();
    live_nulls.apply(&mut entry);
    Ok(entry)
}

// Coalesce only within this read plan and reader generation. Keep selected order
// independently of physical offsets; no payload is retained in the index/cache.
async fn read_entries(selected: Vec<JsonlPayload>) -> SessionResult<Vec<Entry>> {
    let mut entries = vec![None; selected.len()];
    let mut locations = Vec::new();
    // Consume resident references before any I/O so a retired plan does not
    // retain all original bodies alongside their defensive copies.
    for (slot, payload) in selected.into_iter().enumerate() {
        match payload {
            JsonlPayload::Resident(entry) => entries[slot] = Some((*entry).clone()),
            JsonlPayload::Cold {
                source,
                parent,
                live_nulls,
            } => {
                locations.push((slot, source, parent, live_nulls));
            }
        }
    }
    locations.sort_unstable_by_key(|(_, loc, _, _)| {
        (Arc::as_ptr(&loc.reader) as *const () as usize, loc.offset)
    });
    let overflow = || SessionError::storage("cold history range overflow");
    let mut start = 0;
    while start < locations.len() {
        let (_, first, _, _) = &locations[start];
        let mut end = start + 1;
        let mut range_end = first
            .offset
            .checked_add(u64::try_from(first.length).map_err(|_| overflow())?)
            .ok_or_else(overflow)?;
        while end < locations.len() {
            let (_, next, _, _) = &locations[end];
            let next_end = next
                .offset
                .checked_add(u64::try_from(next.length).map_err(|_| overflow())?)
                .ok_or_else(overflow)?;
            let combined_end = range_end.max(next_end);
            if !Arc::ptr_eq(&first.reader, &next.reader)
                || next.offset.saturating_sub(range_end) > 8192
                || combined_end.saturating_sub(first.offset) > 256 * 1024
            {
                break;
            }
            range_end = combined_end;
            end += 1;
        }
        let length = usize::try_from(range_end - first.offset).map_err(|_| overflow())?;
        let bytes = read_range_bounded(&first.reader, first.offset, length).await?;
        if bytes.len() != length {
            return Err(SessionError::storage("short cold history read"));
        }
        for (slot, loc, parent, live_nulls) in &locations[start..end] {
            let offset = usize::try_from(loc.offset - first.offset).map_err(|_| overflow())?;
            let end = offset.checked_add(loc.length).ok_or_else(overflow)?;
            let raw = bytes.get(offset..end).ok_or_else(overflow)?;
            entries[*slot] = Some(restore_entry(loc.decode(raw)?, parent, live_nulls)?);
        }
        start = end;
    }
    entries
        .into_iter()
        .map(|entry| entry.ok_or_else(|| SessionError::storage("missing selected entry")))
        .collect()
}

/// No body/token cache: a plan owns only selected payload references and an
/// exact projection offset. All I/O and canonical projection occur lock-free.
pub(super) struct ContextUsagePlan {
    selected: Vec<JsonlPayload>,
    message_offset: usize,
}
impl ContextUsagePlan {
    pub async fn read(self) -> SessionResult<crate::compaction::ContextUsageEstimate> {
        use crate::compaction::{estimate_context_tokens, ContextUsageEstimate};
        let mut estimate = ContextUsageEstimate::default();
        let mut offset = self.message_offset;
        let mut selected = self.selected.into_iter().peekable();
        while let Some(first) = selected.next() {
            // Resident entries and oversized cold rows are always handled alone.
            let mut bytes = match &first {
                JsonlPayload::Cold { source, .. } => Some(source.length),
                JsonlPayload::Resident(_) => None,
            };
            let mut batch = vec![first];
            while batch.len() < 16 {
                let (Some(total), Some(JsonlPayload::Cold { source, .. })) =
                    (bytes, selected.peek())
                else {
                    break;
                };
                if total > 256 * 1024 || source.length > 256 * 1024 - total {
                    break;
                }
                bytes = Some(total + source.length);
                batch.push(selected.next().unwrap());
            }
            for entry in read_entries(batch).await? {
                let messages = crate::session::context::build_session_context_owned(
                    vec![entry],
                    &Default::default(),
                )
                .messages;
                let group = estimate_context_tokens(&messages);
                if let Some(index) = group.last_usage_index {
                    estimate = ContextUsageEstimate {
                        last_usage_index: Some(offset + index),
                        ..group
                    };
                } else {
                    estimate.trailing_tokens += group.trailing_tokens;
                    estimate.tokens = estimate.usage_tokens + estimate.trailing_tokens;
                }
                offset += messages.len();
                // The row and projected message bodies are dropped each iteration.
            }
        }
        Ok(estimate)
    }
}

pub(super) struct TranscriptPlan {
    selected: Vec<JsonlPayload>,
    total: usize,
    has_more: bool,
    leaf_id: Option<String>,
}
impl TranscriptPlan {
    pub async fn read(self) -> SessionResult<TranscriptPage> {
        Ok(TranscriptPage {
            entries: read_entries(self.selected).await?,
            total: self.total,
            has_more: self.has_more,
            leaf_id: self.leaf_id,
        })
    }
}

pub(super) struct EntryPlan {
    selected: Vec<JsonlPayload>,
    query: EntryQuery,
}
impl EntryPlan {
    pub async fn read(self) -> SessionResult<Vec<Entry>> {
        // Metadata selection already applied limit. Payload predicates must
        // retain their original early stop: do not decode unconsumed rows.
        if self.query.tool_call_id.is_none() && self.query.outbox_key.is_none() {
            return read_entries(self.selected).await;
        }
        let mut entries = Vec::new();
        for payload in self.selected {
            let entry = payload.read().await?;
            if matches_entry_payload(&entry, &self.query) {
                entries.push(entry);
                if self.query.limit.is_some_and(|limit| entries.len() >= limit) {
                    break;
                }
            }
        }
        Ok(entries)
    }
}
pub(super) struct ContextPlan {
    selected: Vec<JsonlPayload>,
    facts: crate::session::context::SessionContext,
}
impl ContextPlan {
    pub async fn read(mut self) -> SessionResult<crate::session::context::SessionContext> {
        let entries = read_entries(self.selected).await?;
        self.facts.messages =
            crate::session::context::build_session_context_owned(entries, &Default::default())
                .messages;
        Ok(self.facts)
    }
}
pub(super) struct SummaryPlan {
    selected: Vec<JsonlPayload>,
    has_more: bool,
    leaf_id: Option<String>,
}
impl SummaryPlan {
    pub async fn read(self) -> SessionResult<MessageSummaryPage> {
        let mut messages = Vec::new();
        for payload in self.selected {
            let entry = payload.read().await?;
            let Some(rpi_agent::message::AgentMessage::User(user)) = entry.as_message() else {
                return Err(SessionError::invalid_entry("user index invariant"));
            };
            let (preview, has_images) = crate::session::message_preview::preview(&user.content);
            messages.push(MessageSummary {
                entry_id: entry.id().into(),
                role: "user".into(),
                preview,
                timestamp: user.timestamp,
                has_images,
            });
        }
        let first_id = messages.first().map(|m| m.entry_id.clone());
        let last_id = messages.last().map(|m| m.entry_id.clone());
        Ok(MessageSummaryPage {
            filtered: true,
            role: "user".into(),
            next_before_id: self.has_more.then(|| first_id.clone()).flatten(),
            first_id,
            last_id,
            messages,
            has_more: self.has_more,
            leaf_id: self.leaf_id,
        })
    }
}
pub(super) struct WindowPlan {
    selected: Vec<JsonlPayload>,
    before: bool,
    after: bool,
    leaf_id: Option<String>,
}
impl WindowPlan {
    pub async fn read(self) -> SessionResult<MessageWindowPage> {
        let mut entries = Vec::new();
        for payload in self.selected {
            entries.push(payload.read().await?);
        }
        let first_id = entries.first().map(|e| e.id().to_owned());
        let last_id = entries.last().map(|e| e.id().to_owned());
        Ok(MessageWindowPage {
            next_before_id: self.before.then(|| first_id.clone()).flatten(),
            next_after_id: self.after.then(|| last_id.clone()).flatten(),
            first_id,
            last_id,
            entries,
            has_more_before: self.before,
            has_more_after: self.after,
            leaf_id: self.leaf_id,
        })
    }
}
enum Selected<T> {
    Hot(T),
    Cold(Arc<Location>),
}
impl<T> Selected<T> {
    fn seq(&self, hot: impl FnOnce(&T) -> u64) -> u64 {
        match self {
            Self::Hot(v) => hot(v),
            Self::Cold(loc) => loc.seq,
        }
    }
}
pub(super) struct RecordPlan {
    selected: Vec<Selected<LaneRecord>>,
}
enum LogSelected {
    Hot(LogItem),
    Entry(u64, JsonlPayload),
    Cold(Arc<Location>),
}
impl LogSelected {
    fn seq(&self) -> u64 {
        match self {
            Self::Hot(item) => item.seq(),
            Self::Entry(seq, _) => *seq,
            Self::Cold(loc) => loc.seq,
        }
    }
}
pub(super) struct LogPlan {
    selected: Vec<LogSelected>,
}
// Coalesce selected frame ranges from the same reader generation, without a
// retained cache. A large individual row is permitted; combined reads are bounded.
async fn read_frames(locations: Vec<Arc<Location>>) -> SessionResult<HashMap<u64, LaneRecord>> {
    let mut locations = locations;
    locations
        .sort_unstable_by_key(|loc| (Arc::as_ptr(&loc.reader) as *const () as usize, loc.offset));
    let mut records = HashMap::new();
    let mut start = 0;
    while start < locations.len() {
        let first = &locations[start];
        let mut end = start + 1;
        let mut range_end = first.offset + first.length as u64;
        while end < locations.len() {
            let next = &locations[end];
            let next_end = next.offset + next.length as u64;
            if !Arc::ptr_eq(&first.reader, &next.reader)
                || next.offset.saturating_sub(range_end) > 8192
                || next_end.saturating_sub(first.offset) > 256 * 1024
            {
                break;
            }
            range_end = next_end;
            end += 1;
        }
        let length = usize::try_from(range_end - first.offset)
            .map_err(|_| SessionError::storage("cold history range overflow"))?;
        let bytes = read_range_bounded(&first.reader, first.offset, length).await?;
        if bytes.len() != length {
            return Err(SessionError::storage("short cold history read"));
        }
        for loc in &locations[start..end] {
            let offset = (loc.offset - first.offset) as usize;
            let SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame),
            } = loc.decode(&bytes[offset..offset + loc.length])?
            else {
                return Err(SessionError::invalid_entry(
                    "cold history frame identity mismatch",
                ));
            };
            if frame.op == AssistantFrameOp::CommitStream {
                return Err(SessionError::invalid_entry(
                    "cold history frame identity mismatch",
                ));
            }
            records.insert(loc.seq, LaneRecord::AssistantFrame(frame));
        }
        start = end;
    }
    Ok(records)
}
impl RecordPlan {
    pub async fn read(self) -> SessionResult<Vec<LaneRecord>> {
        let mut frames = read_frames(
            self.selected
                .iter()
                .filter_map(|item| {
                    if let Selected::Cold(loc) = item {
                        Some(loc.clone())
                    } else {
                        None
                    }
                })
                .collect(),
        )
        .await?;
        self.selected
            .into_iter()
            .map(|item| match item {
                Selected::Hot(record) => Ok(record),
                Selected::Cold(loc) => frames
                    .remove(&loc.seq)
                    .ok_or_else(|| SessionError::storage("missing cold record")),
            })
            .collect()
    }
    pub async fn page(self, total: usize) -> SessionResult<RecordPage> {
        Ok(RecordPage {
            records: self.read().await?,
            total,
        })
    }
}
impl LogPlan {
    pub async fn read(self) -> SessionResult<Vec<LogItem>> {
        let mut frames = read_frames(
            self.selected
                .iter()
                .filter_map(|item| {
                    if let LogSelected::Cold(loc) = item {
                        Some(loc.clone())
                    } else {
                        None
                    }
                })
                .collect(),
        )
        .await?;
        let mut result = Vec::new();
        for item in self.selected {
            result.push(match item {
                LogSelected::Hot(item) => item,
                LogSelected::Entry(seq, payload) => LogItem::Entry {
                    seq,
                    entry: payload.read().await?,
                },
                LogSelected::Cold(loc) => LogItem::Record {
                    seq: loc.seq,
                    record: frames
                        .remove(&loc.seq)
                        .ok_or_else(|| SessionError::storage("missing cold record"))?,
                },
            });
        }
        Ok(result)
    }
}
#[cfg(test)]
#[path = "context_usage_tests.rs"]
mod context_usage_tests;
#[cfg(test)]
#[path = "cold_frames_tests.rs"]
mod tests;

impl ColdFrames {
    fn push(&mut self, location: Arc<Location>) {
        if location.append {
            if !self.active.contains_key(location.run.as_ref()) {
                self.active.insert(location.run.to_string(), HashMap::new());
            }
            self.active
                .get_mut(location.run.as_ref())
                .expect("active run inserted")
                .entry(location.stream_index)
                .or_default()
                .push(self.locations.len());
        }
        self.locations.push(location);
    }
    fn retire(&mut self, frame: &AssistantFrameRecord) {
        let streams = match frame.op {
            AssistantFrameOp::ClearRun => self.active.remove(&frame.run_id),
            AssistantFrameOp::ClearStream => {
                let Some(streams) = self.active.get_mut(&frame.run_id) else {
                    return;
                };
                let selected = streams.remove(&frame.stream_index);
                if streams.is_empty() {
                    self.active.remove(&frame.run_id);
                }
                selected.map(|positions| HashMap::from([(frame.stream_index, positions)]))
            }
            _ => return,
        };
        // Preserve historical run/stream matching (including across lanes).
        // IDs remain reserved independently of this disposable lookup index.
        if let Some(streams) = streams {
            for positions in streams.into_values() {
                for position in positions {
                    Arc::make_mut(&mut self.locations[position]).retired = true;
                }
            }
        }
    }
}
