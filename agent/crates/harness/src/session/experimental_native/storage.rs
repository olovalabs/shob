use super::engine::{self, Effect, Identity, Meta, Projection, Store};
use crate::{
    error::{SessionError, SessionResult},
    session::{
        memory::{stamp_record, Clock},
        progress::ProgressState,
        types::*,
    },
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::sync::Mutex as AsyncMutex;

type Result<T> = engine::Result<T>;
#[derive(Clone)]
pub struct ExperimentalNativeStorage {
    pub(super) metadata: SessionMetadata,
    pub(super) inner: Arc<Inner>,
}
pub(super) struct Inner {
    pub store: Mutex<Store>,
    pub gate: Arc<AsyncMutex<()>>,
    pub clock: Arc<dyn Clock>,
}
fn session_error(error: Box<dyn std::error::Error>) -> SessionError {
    error
        .downcast_ref::<SessionError>()
        .cloned()
        .unwrap_or_else(|| SessionError::storage(error.to_string()))
}
pub(super) fn auxiliary<T: DeserializeOwned>(
    db: &Connection,
    identity: &Identity,
    key: &str,
) -> Result<T> {
    let (json,hash):(Option<String>,Option<Vec<u8>>)=db.query_row("SELECT CASE WHEN length(CAST(json AS BLOB))<=4194304 THEN json END,CASE WHEN length(digest)=32 THEN digest END FROM auxiliary WHERE key=?1",[key],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let json = json.ok_or("auxiliary state exceeds experimental limit")?;
    engine::ensure(
        hash.as_deref() == Some(engine::digest(identity, 0, key, json.as_bytes()).as_slice()),
        "auxiliary identity/digest mismatch",
    )?;
    Ok(serde_json::from_str(&json)?)
}
pub(super) fn publish_aux<T: Serialize>(
    db: &Connection,
    identity: &Identity,
    key: &str,
    value: &T,
) -> Result<()> {
    let json = serde_json::to_string(value)?;
    engine::ensure(
        json.len() <= 4 * 1024 * 1024,
        "auxiliary state exceeds experimental limit",
    )?;
    db.execute("INSERT INTO auxiliary VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET json=excluded.json,digest=excluded.digest",params![key,json,engine::digest(identity,0,key,json.as_bytes())])?;
    Ok(())
}
#[derive(Clone)]
pub(super) enum Write {
    Entry(ProvisionedEntry, String),
    Record(LaneRecord),
    Lane(String, Option<String>, bool),
    Name(Option<String>),
    Label(String, Option<String>),
    Raw(SessionMutation),
    Cancellation(LaneRecord),
}
impl ExperimentalNativeStorage {
    pub(super) fn from_store(
        store: Store,
        metadata: SessionMetadata,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            metadata,
            inner: Arc::new(Inner {
                store: Mutex::new(store),
                gate: Arc::new(AsyncMutex::new(())),
                clock,
            }),
        }
    }
    async fn run<T: Send + 'static>(
        &self,
        op: impl FnOnce(&mut Store, &dyn Clock) -> Result<T> + Send + 'static,
    ) -> SessionResult<T> {
        // Cancellation before admission starts nothing. Once admitted, the blocking owner
        // retains the gate even when its caller drops the JoinHandle/future.
        let guard = self.inner.gate.clone().lock_owned().await;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let mut store = inner
                .store
                .lock()
                .map_err(|_| SessionError::storage("experimental writer mutex poisoned"))?;
            op(&mut store, inner.clock.as_ref()).map_err(session_error)
        })
        .await
        .map_err(|_| SessionError::storage("experimental blocking owner failed"))?
    }
    async fn write(
        &self,
        writes: Vec<Write>,
        expected: Option<u64>,
    ) -> SessionResult<Vec<SessionMutation>> {
        self.run(move |store, clock| commit(store, writes, expected, clock))
            .await
    }
}

fn commit(
    store: &mut Store,
    writes: Vec<Write>,
    expected: Option<u64>,
    clock: &dyn Clock,
) -> Result<Vec<SessionMutation>> {
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let result = apply_writes(&tx, &store.identity, writes, expected, clock)?;
    #[cfg(test)]
    if let Some(probe) = &store.process_probe {
        probe("history:before");
    }
    tx.commit()?;
    #[cfg(test)]
    if let Some(probe) = &store.process_probe {
        probe("history:after");
    }
    Ok(result)
}
pub(super) fn apply_writes(
    tx: &Connection,
    identity: &Identity,
    writes: Vec<Write>,
    expected: Option<u64>,
    clock: &dyn Clock,
) -> Result<Vec<SessionMutation>> {
    let (mut state, _) = engine::restore(tx, identity)?;
    if expected.is_some_and(|e| e != state.seq) {
        return Err(SessionError::storage("Session history revision changed").into());
    }
    let mut checkpoint = engine::read_checkpoint(tx, identity)?.0.seq;
    let mut committed = Vec::new();
    for write in writes {
        let seq = state
            .seq
            .checked_add(1)
            .filter(|s| *s <= i64::MAX as u64)
            .ok_or("native sequence overflow")?;
        let mutation = match write {
            Write::Entry(entry, lane) => {
                let parent = state
                    .heads
                    .get(&lane)
                    .cloned()
                    .ok_or_else(|| SessionError::invalid_lane("lane not found"))?;
                let timestamp = clock.now_ms();
                let entry = provisioned_into_entry(entry, seq, parent, timestamp);
                SessionMutation::Entry {
                    seq,
                    timestamp,
                    lane: Some(lane),
                    entry,
                }
            }
            Write::Record(record) => {
                require_record(tx, &state, &record)?;
                if matches!(record, LaneRecord::OperationStarted(_))
                    && state.open.get(record.lane()).is_some_and(|o| !o.is_empty())
                {
                    return Err(SessionError::storage("lane already has an open operation").into());
                }
                let lane = record.lane().to_owned();
                SessionMutation::Record {
                    record: stamp_record(record, seq, lane, clock.now_ms()),
                }
            }
            Write::Lane(lane, leaf, create) => {
                if create && state.heads.contains_key(&lane) {
                    return Err(SessionError::already_exists("lane already exists").into());
                }
                if !create && !state.heads.contains_key(&lane) {
                    return Err(SessionError::invalid_lane("lane not found").into());
                }
                if let Some(target) = &leaf {
                    require_target(tx, &state, target)?;
                }
                SessionMutation::Lane {
                    seq,
                    lane,
                    leaf_id: leaf,
                }
            }
            Write::Name(name) => SessionMutation::FactName { seq, name },
            Write::Label(target_id, label) => {
                require_target(tx, &state, &target_id)?;
                SessionMutation::FactLabel {
                    seq,
                    target_id,
                    label,
                }
            }
            Write::Cancellation(record) => {
                require_record(tx, &state, &record)?;
                let LaneRecord::QueueCancelled(cancel) = &record else {
                    return Err(SessionError::storage("invalid cancellation batch").into());
                };
                let latest = state
                    .pending
                    .get(&cancel.base.lane)
                    .into_iter()
                    .flatten()
                    .rev()
                    .find(|p| p.target == cancel.entry_id);
                let matches = match latest {
                    Some(p) => {
                        engine::read_metadata(tx, identity, p.reference.seq)?
                            .0
                            .selection
                            .run
                            == cancel.run_id
                    }
                    None => false,
                };
                if !matches {
                    return Err(SessionError::storage(
                        "queue cancellation has no pending matching enqueue",
                    )
                    .into());
                }
                let lane = record.lane().to_owned();
                SessionMutation::Record {
                    record: stamp_record(record, seq, lane, clock.now_ms()),
                }
            }
            Write::Raw(mutation) => mutation,
        };
        if let SessionMutation::Entry { entry, .. } = &mutation {
            if exists_id(tx, entry.id())? {
                return Err(
                    SessionError::already_exists("entry/record identity already exists").into(),
                );
            }
        }
        if let SessionMutation::Record { record } = &mutation {
            if exists_id(tx, record.id())? {
                return Err(
                    SessionError::already_exists("entry/record identity already exists").into(),
                );
            }
        }
        engine::insert_fact(tx, identity, &mut state, &mutation)?;
        if state.seq - checkpoint >= engine::TAIL {
            engine::publish(tx, identity, &state)?;
            checkpoint = state.seq;
        }
        committed.push(mutation);
    }
    tx.execute("UPDATE root SET revision=?1 WHERE singleton=1", [state.seq])?;
    Ok(committed)
}
fn require_target(db: &Connection, state: &Projection, id: &str) -> Result<()> {
    if !engine::exists_entry(db, id, state.seq)? {
        return Err(SessionError::not_found("entry target not found").into());
    }
    Ok(())
}
fn require_record(db: &Connection, state: &Projection, record: &LaneRecord) -> Result<()> {
    if !state.heads.contains_key(record.lane()) {
        return Err(SessionError::invalid_lane("record lane not found").into());
    }
    if exists_id(db, record.id())? {
        return Err(SessionError::already_exists("entry/record identity already exists").into());
    }
    Ok(())
}
fn exists_id(db: &Connection, id: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM facts WHERE id=?1)",
        [id],
        |r| r.get(0),
    )?)
}
fn limit(limit: Option<usize>) -> Result<()> {
    if limit == Some(0) {
        Err(SessionError::invalid_query("limit must be positive").into())
    } else {
        Ok(())
    }
}
fn entry_at(db: &Connection, identity: &Identity, id: &str) -> Result<Option<(u64, Meta)>> {
    let seq = db
        .query_row(
            "SELECT seq FROM facts WHERE id=?1 AND category='entry'",
            [id],
            |r| r.get::<_, u64>(0),
        )
        .optional()?;
    seq.map(|seq| Ok((seq, engine::read_metadata(db, identity, seq)?.0)))
        .transpose()
}
fn metas(db: &Connection, identity: &Identity) -> Result<Vec<Meta>> {
    let mut statement = db.prepare("SELECT seq FROM facts ORDER BY seq")?;
    let rows = statement.query_map([], |r| r.get::<_, u64>(0))?;
    rows.map(|row| Ok(engine::read_metadata(db, identity, row?)?.0))
        .collect()
}
fn selected_entries(
    db: &Connection,
    identity: &Identity,
    q: &EntryQuery,
    path: Option<Vec<Meta>>,
) -> Result<Vec<Entry>> {
    limit(q.limit)?;
    let Some(keys) = path else {
        return super::queries::entries(db, identity, q);
    };
    let mut selected = keys
        .into_iter()
        .filter(|m| match &m.effect {
            Effect::Entry { id, .. } => m.selection.entry_matches(id, m.seq, q),
            _ => false,
        })
        .collect::<Vec<_>>();
    selected.sort_by_key(|m| m.seq);
    if q.order != Some(EntryOrder::OldestFirst) {
        selected.reverse();
    }
    selected.truncate(q.limit.unwrap_or(usize::MAX));
    selected
        .into_iter()
        .map(|m| match engine::read_mutation(db, identity, m.seq)? {
            SessionMutation::Entry { entry, .. } => Ok(entry),
            _ => Err("invalid entry selection".into()),
        })
        .collect()
}
fn records(db: &Connection, identity: &Identity, q: &RecordQuery) -> Result<Vec<LaneRecord>> {
    limit(q.limit)?;
    if !q.pending_queue_only {
        return super::queries::records(db, identity, q);
    }
    let mut keys = if q.pending_queue_only {
        let (state, _) = engine::restore(db, identity)?;
        state
            .pending
            .values()
            .flatten()
            .map(|p| engine::read_metadata(db, identity, p.reference.seq).map(|r| r.0))
            .collect::<Result<Vec<_>>>()?
    } else {
        metas(db, identity)?
    };
    keys.retain(
        |m| matches!(&m.effect,Effect::Record{lane,..} if m.selection.record_matches(lane,m.seq,q)),
    );
    keys.sort_by_key(|m| m.seq);
    if q.order != Some(EntryOrder::OldestFirst) {
        keys.reverse();
    }
    keys.truncate(q.limit.unwrap_or(usize::MAX));
    keys.into_iter()
        .map(|m| match engine::read_mutation(db, identity, m.seq)? {
            SessionMutation::Record { record } => Ok(record),
            _ => Err("invalid record selection".into()),
        })
        .collect()
}
fn path(
    db: &Connection,
    identity: &Identity,
    start: &str,
    bounds: &BranchBounds,
) -> Result<Vec<Meta>> {
    let mut next = Some(start.to_owned());
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    while let Some(id) = next {
        if !seen.insert(id.clone()) {
            return Err(SessionError::invalid_entry("branch cycle").into());
        }
        let (_, meta) = entry_at(db, identity, &id)?
            .ok_or_else(|| SessionError::not_found("branch entry missing"))?;
        let stop = bounds.stop_at_id.as_deref() == Some(&id)
            || bounds
                .stop_at_type
                .is_some_and(|t| meta.selection.entry_type.as_deref() == Some(t));
        next = match &meta.effect {
            Effect::Entry { parent, .. } => parent.clone(),
            _ => return Err("invalid parent metadata".into()),
        };
        result.push(meta);
        if stop {
            break;
        }
    }
    Ok(result)
}
#[async_trait::async_trait]
impl SessionStorage for ExperimentalNativeStorage {
    fn metadata(&self) -> SessionMetadata {
        self.metadata.clone()
    }
    async fn drain(&self) {
        let _guard = self.inner.gate.lock().await;
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.run(|s, _| {
            Ok(s.restore()?
                .0
                .heads
                .into_iter()
                .map(|(lane, leaf_id)| LanePointer { lane, leaf_id })
                .collect())
        })
        .await
    }
    async fn history_revision(&self) -> SessionResult<u64> {
        self.run(|s, _| Ok(s.restore()?.0.seq)).await
    }
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.write(
            vec![Write::Lane(lane.into(), at.map(str::to_owned), true)],
            None,
        )
        .await?;
        Ok(())
    }
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.write(
            vec![Write::Lane(lane.into(), to.map(str::to_owned), false)],
            None,
        )
        .await?;
        Ok(())
    }
    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.append_entry_impl(entry, lane, None).await
    }
    async fn append_entry_if_revision(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
        expected: u64,
    ) -> SessionResult<Entry> {
        self.append_entry_impl(entry, lane, Some(expected)).await
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.append_record_impl(record, None).await
    }
    async fn append_record_if_revision(
        &self,
        record: LaneRecord,
        expected: u64,
    ) -> SessionResult<LaneRecord> {
        self.append_record_impl(record, Some(expected)).await
    }
    async fn append_records(&self, records: Vec<LaneRecord>) -> SessionResult<Vec<LaneRecord>> {
        Ok(self
            .write(records.into_iter().map(Write::Record).collect(), None)
            .await?
            .into_iter()
            .filter_map(|m| {
                if let SessionMutation::Record { record } = m {
                    Some(record)
                } else {
                    None
                }
            })
            .collect())
    }
    async fn append_queue_cancellations(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        let mut ids = HashSet::new();
        let mut targets = HashSet::new();
        for record in &records {
            match record {
                LaneRecord::QueueCancelled(r) => {
                    if !ids.insert(r.base.id.clone()) || !targets.insert(r.entry_id.clone()) {
                        return Err(SessionError::storage("duplicate queue cancellation"));
                    }
                }
                _ => {
                    return Err(SessionError::storage(
                        "non cancellation in cancellation batch",
                    ))
                }
            }
        }
        Ok(self
            .write(records.into_iter().map(Write::Cancellation).collect(), None)
            .await?
            .into_iter()
            .filter_map(|m| match m {
                SessionMutation::Record { record } => Some(record),
                _ => None,
            })
            .collect())
    }
    async fn append_record_and_entry(
        &self,
        record: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
    ) -> SessionResult<(LaneRecord, Entry)> {
        let mut results = self
            .write(
                vec![Write::Record(record), Write::Entry(entry, lane.into())],
                None,
            )
            .await?
            .into_iter();
        match (results.next(), results.next()) {
            (
                Some(SessionMutation::Record { record }),
                Some(SessionMutation::Entry { entry, .. }),
            ) => Ok((record, entry)),
            _ => Err(SessionError::storage("invalid atomic append result")),
        }
    }
    async fn append_record_entry_and_record(
        &self,
        before: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
        after: LaneRecord,
    ) -> SessionResult<(LaneRecord, Entry, LaneRecord)> {
        let mut results = self
            .write(
                vec![
                    Write::Record(before),
                    Write::Entry(entry, lane.into()),
                    Write::Record(after),
                ],
                None,
            )
            .await?
            .into_iter();
        match (results.next(), results.next(), results.next()) {
            (
                Some(SessionMutation::Record { record: before }),
                Some(SessionMutation::Entry { entry, .. }),
                Some(SessionMutation::Record { record: after }),
            ) => Ok((before, entry, after)),
            _ => Err(SessionError::storage("invalid atomic append result")),
        }
    }
    async fn message_summaries(
        &self,
        lane: &str,
        query: &MessageSummaryQuery,
    ) -> SessionResult<MessageSummaryPage> {
        let (lane, query) = (lane.to_owned(), query.clone());
        self.run(move |s, _| {
            let tx = s.connection.transaction()?;
            let result = super::pages::summaries(&tx, &s.identity, &lane, &query)?;
            tx.commit()?;
            Ok(result)
        })
        .await
    }
    async fn message_window(
        &self,
        lane: &str,
        query: &MessageWindowQuery,
    ) -> SessionResult<MessageWindowPage> {
        let (lane, query) = (lane.to_owned(), query.clone());
        self.run(move |s, _| {
            let tx = s.connection.transaction()?;
            let result = super::pages::window(&tx, &s.identity, &lane, &query)?;
            tx.commit()?;
            Ok(result)
        })
        .await
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        let id = id.to_owned();
        self.run(move |s, _| {
            let tx = s.connection.transaction()?;
            let result = match entry_at(&tx, &s.identity, &id)? {
                Some((seq, _)) => match engine::read_mutation(&tx, &s.identity, seq)? {
                    SessionMutation::Entry { entry, .. } => Some(entry),
                    _ => return Err("invalid entry identity".into()),
                },
                None => None,
            };
            tx.commit()?;
            Ok(result)
        })
        .await
    }
    async fn find_entries(&self, q: &EntryQuery) -> SessionResult<Vec<Entry>> {
        let q = q.clone();
        self.run(move |s, _| {
            let tx = s.connection.transaction()?;
            let r = selected_entries(&tx, &s.identity, &q, None)?;
            tx.commit()?;
            Ok(r)
        })
        .await
    }
    async fn find_entries_on_branch(
        &self,
        q: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        let (q, bounds, start) = (q.clone(), bounds.clone(), start.to_owned());
        self.run(move |s, _| {
            let tx = s.connection.transaction()?;
            let mut keys = path(&tx, &s.identity, &start, &bounds)?;
            // Preserve SessionState's inclusive-bound behavior in oldest-first scans.
            if q.order == Some(EntryOrder::OldestFirst)
                && keys.last().is_some_and(|m| {
                    bounds.stop_at_id.as_deref() == m.id()
                        || bounds
                            .stop_at_type
                            .is_some_and(|t| m.selection.entry_type.as_deref() == Some(t))
                })
            {
                keys = keys.into_iter().rev().take(1).collect();
            }
            let r = selected_entries(&tx, &s.identity, &q, Some(keys))?;
            tx.commit()?;
            Ok(r)
        })
        .await
    }
    async fn branch_ancestry(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<EntryAncestry>> {
        let (bounds, start) = (bounds.clone(), start.to_owned());
        self.run(move |s, _| {
            let tx = s.connection.transaction()?;
            // path() validates metadata digests and parent links only; it never
            // calls read_mutation or selects the facts.body column.
            let ancestry = path(&tx, &s.identity, &start, &bounds)?
                .into_iter()
                .map(|meta| match meta.effect {
                    Effect::Entry { id, parent, .. } => Ok(EntryAncestry {
                        id,
                        parent_id: parent,
                    }),
                    _ => Err("invalid ancestry metadata".into()),
                })
                .collect::<Result<Vec<_>>>()?;
            tx.commit()?;
            Ok(ancestry)
        })
        .await
    }
    async fn find_records(&self, q: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        let q = q.clone();
        self.run(move |s, _| {
            let tx = s.connection.transaction()?;
            let r = records(&tx, &s.identity, &q)?;
            tx.commit()?;
            Ok(r)
        })
        .await
    }
    async fn record_page(&self, q: &RecordPageQuery) -> SessionResult<RecordPage> {
        let q = q.clone();
        self.run(move |s, _| {
            limit(Some(q.limit))?;
            let tx = s.connection.transaction()?;
            let page = super::queries::page(&tx, &s.identity, &q)?;
            tx.commit()?;
            Ok(page)
        })
        .await
    }
    async fn find_open_operations(
        &self,
        lane: &str,
        cap: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        let lane = lane.to_owned();
        self.run(move |s, _| {
            limit(cap)?;
            let tx = s.connection.transaction()?;
            let (state, _) = engine::restore(&tx, &s.identity)?;
            let result = state
                .open
                .get(&lane)
                .into_iter()
                .flatten()
                .rev()
                .take(cap.unwrap_or(usize::MAX))
                .map(|r| match engine::read_mutation(&tx, &s.identity, r.seq)? {
                    SessionMutation::Record {
                        record: LaneRecord::OperationStarted(record),
                    } => Ok(record),
                    _ => Err("invalid open operation reference".into()),
                })
                .collect::<Result<Vec<_>>>()?;
            tx.commit()?;
            Ok(result)
        })
        .await
    }
    async fn get_log(&self, q: &LogOptions) -> SessionResult<Vec<LogItem>> {
        let q = q.clone();
        self.run(move |s, _| {
            limit(q.limit)?;
            let tx = s.connection.transaction()?;
            let keys = super::queries::log_keys(&tx, &s.identity, &q)?;
            let result = keys
                .into_iter()
                .map(|seq| Ok(to_log(engine::read_mutation(&tx, &s.identity, seq)?)))
                .collect::<Result<Vec<_>>>()?;
            tx.commit()?;
            Ok(result)
        })
        .await
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        self.run(|s, _| s.name()).await
    }
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.write(vec![Write::Name(name.map(str::to_owned))], None)
            .await?;
        Ok(())
    }
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        let id = id.to_owned();
        self.run(move |s, _| s.label(&id)).await
    }
    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        self.write(
            vec![Write::Label(id.into(), label.map(str::to_owned))],
            None,
        )
        .await?;
        Ok(())
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.run(|s, _| Ok(s.restore()?.0.stats)).await
    }
    async fn append_progress(
        &self,
        records: Vec<AssistantFrameRecord>,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.run(move |s, clock| {
            let tx = s
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (state, _) = engine::restore(&tx, &s.identity)?;
            for record in &records {
                if !state.heads.contains_key(&record.base.lane) {
                    return Err(SessionError::invalid_lane("missing progress lane").into());
                }
            }
            let mut progress: ProgressState = auxiliary(&tx, &s.identity, "progress")?;
            progress.validate().map_err(SessionError::from)?;
            let prepared = progress
                .prepare(records, || clock.now_ms())
                .map_err(SessionError::from)?;
            progress.apply(&prepared);
            publish_aux(&tx, &s.identity, "progress", &progress)?;
            #[cfg(test)]
            if let Some(probe) = &s.process_probe {
                probe("progress:before");
            }
            tx.commit()?;
            #[cfg(test)]
            if let Some(probe) = &s.process_probe {
                probe("progress:after");
            }
            Ok(prepared)
        })
        .await
    }
    async fn read_progress(&self, run_id: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
        let run_id = run_id.to_owned();
        self.run(move |s, _| {
            let mut progress: ProgressState = auxiliary(&s.connection, &s.identity, "progress")?;
            progress.validate().map_err(SessionError::from)?;
            Ok(progress.read(&run_id))
        })
        .await
    }
    async fn prune_completed_progress(&self) -> SessionResult<usize> {
        self.run(|s, _| {
            let tx = s.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut progress: ProgressState = auxiliary(&tx, &s.identity, "progress")?;
            progress.validate().map_err(SessionError::from)?;
            let mut completed = crate::session::progress::CompletedProgressRuns::new();
            let scopes = progress.frames.iter().map(|f| (f.base.lane.as_str(), f.run_id.as_str())).collect::<HashSet<_>>();
            // Probe only active scopes; never materialize old terminal bodies. Validate
            // the selected terminal metadata before allowing destructive retirement.
            for (lane, run) in scopes {
                let seq = tx.query_row(
                    "SELECT seq FROM facts WHERE lane=?1 AND category='finish' AND target=?2 ORDER BY seq DESC LIMIT 1",
                    params![lane,run], |r| r.get::<_,u64>(0),
                ).optional()?;
                if let Some(seq)=seq {
                    let meta=engine::read_metadata(&tx,&s.identity,seq)?.0;
                    engine::ensure(matches!(meta.effect, Effect::Record{lane:ref actual,record:engine::RecordEffect::Finish{run:ref actual_run},..} if actual==lane && actual_run==run), "invalid terminal proof")?;
                    completed.entry(lane.to_owned()).or_default().insert(run.to_owned());
                }
            }
            let removed=progress.prune_completed(&completed);
            if removed>0 {publish_aux(&tx,&s.identity,"progress",&progress)?;}
            #[cfg(test)]
            if let Some(probe) = &s.process_probe { probe("prune:before"); }
            tx.commit()?;
            #[cfg(test)]
            if let Some(probe) = &s.process_probe { probe("prune:after"); }
            Ok(removed)
        }).await
    }
}
impl ExperimentalNativeStorage {
    async fn append_entry_impl(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
        expected: Option<u64>,
    ) -> SessionResult<Entry> {
        match self
            .write(vec![Write::Entry(entry, lane.into())], expected)
            .await?
            .pop()
        {
            Some(SessionMutation::Entry { entry, .. }) => Ok(entry),
            _ => Err(SessionError::storage("invalid entry result")),
        }
    }
    async fn append_record_impl(
        &self,
        record: LaneRecord,
        expected: Option<u64>,
    ) -> SessionResult<LaneRecord> {
        match self
            .write(vec![Write::Record(record)], expected)
            .await?
            .pop()
        {
            Some(SessionMutation::Record { record }) => Ok(record),
            _ => Err(SessionError::storage("invalid record result")),
        }
    }
    pub(super) async fn fork_mutations(
        &self,
        fork: ForkOptions,
    ) -> SessionResult<Vec<SessionMutation>> {
        self.run(move |s, _| {
            let tx = s.connection.transaction()?;
            let mut state = crate::session::state::SessionState::new();
            for meta in metas(&tx, &s.identity)? {
                state.apply_mutation(engine::read_mutation(&tx, &s.identity, meta.seq)?)?;
            }
            let result = state.create_fork_mutations(&fork)?;
            tx.commit()?;
            Ok(result)
        })
        .await
    }
}
fn to_log(m: SessionMutation) -> LogItem {
    match m {
        SessionMutation::Entry { seq, entry, .. } => LogItem::Entry { seq, entry },
        SessionMutation::Record { record } => LogItem::Record {
            seq: record.seq(),
            record,
        },
        SessionMutation::Lane { seq, lane, leaf_id } => LogItem::Lane { seq, lane, leaf_id },
        SessionMutation::FactName { seq, name } => LogItem::FactName { seq, name },
        SessionMutation::FactLabel {
            seq,
            target_id,
            label,
        } => LogItem::FactLabel {
            seq,
            target_id,
            label,
        },
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use crate::session::experimental_native::ExperimentalNativeRepo;
    #[tokio::test]
    async fn admitted_owner_commits_after_caller_cancellation_and_drain_waits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
            .await
            .unwrap();
        let storage = repo.create(&SessionCreateOptions::default()).await.unwrap();
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = storage.clone();
        let task = tokio::spawn(async move {
            worker
                .run(move |store, clock| {
                    start_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    commit(
                        store,
                        vec![Write::Name(Some("committed after cancellation".into()))],
                        None,
                        clock,
                    )
                })
                .await
        });
        tokio::task::spawn_blocking(move || start_rx.recv().unwrap())
            .await
            .unwrap();
        task.abort();
        let _ = task.await;
        let draining = storage.drain();
        tokio::pin!(draining);
        assert!(
            futures::poll!(&mut draining).is_pending(),
            "drain must wait for the admitted owner"
        );
        release_tx.send(()).unwrap();
        draining.await;
        assert_eq!(
            storage.get_name().await.unwrap().as_deref(),
            Some("committed after cancellation")
        );
    }
    #[tokio::test]
    async fn cancellation_before_admission_never_starts_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
            .await
            .unwrap();
        let storage = repo.create(&SessionCreateOptions::default()).await.unwrap();
        let guard = storage.inner.gate.clone().lock_owned().await;
        let mut write = Box::pin(storage.set_name(Some("must not commit")));
        assert!(futures::poll!(&mut write).is_pending());
        drop(write);
        drop(guard);
        storage.drain().await;
        assert_eq!(storage.get_name().await.unwrap(), None);
    }
}

#[cfg(test)]
#[path = "indexed_tests.rs"]
mod indexed_tests;
#[cfg(test)]
#[path = "process_tests.rs"]
mod process_tests;
#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
