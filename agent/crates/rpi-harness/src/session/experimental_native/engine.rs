//! Shared experimental native fact engine. Default host paths never load it.
use super::facts::{jsonl::encode_mutation, types::*};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Read,
    path::Path,
    time::Duration,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
// Keep atomic path-reservation failures distinct from initializer/SQLite errors.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct ReservationError(#[source] pub(super) std::io::Error);
pub const FORMAT: &str = "rpi-native-fact-experimental-v3";
pub const TAIL: u64 = 16;
const APP: i64 = 0x52504951;
pub(super) const META_LIMIT: usize = 16384;
const CHECKPOINT_LIMIT: usize = 256 * 1024;
pub(super) const BODY_LIMIT: usize = 4 * 1024 * 1024;
pub(super) fn ensure(ok: bool, reason: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(reason.into())
    }
}

/// Admit only the initialized experimental format before SQLite can replay a
/// journal. This is a bounded format-family gate, not authentication or fencing:
/// a recognized but internally damaged artifact may still undergo SQLite recovery
/// before the later schema/root/digest checks reject it.
pub(super) fn validate_header(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    ensure(
        file.metadata()?.is_file(),
        "native artifact must be a regular file",
    )?;
    let mut header = [0u8; 100];
    file.read_exact(&mut header)?;
    let page = u16::from_be_bytes([header[16], header[17]]);
    ensure(
        &header[..16] == b"SQLite format 3\0"
            && u32::from_be_bytes(header[60..64].try_into()?) == 3
            && u32::from_be_bytes(header[68..72].try_into()?) == APP as u32
            && (page == 1 || ((512..=32768).contains(&page) && page.is_power_of_two()))
            && matches!(header[18], 1 | 2)
            && matches!(header[19], 1 | 2),
        "unsupported native fact format",
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub session: String,
    pub generation: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub seq: u64,
    pub effect: Effect,
    #[serde(default)]
    pub selection: super::selection::Selection,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Effect {
    Entry {
        id: String,
        lane: Option<String>,
        parent: Option<String>,
        message: bool,
    },
    Record {
        id: String,
        lane: String,
        record: RecordEffect,
    },
    Lane {
        lane: String,
        leaf: Option<String>,
    },
    Name,
    Label {
        target: String,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum RecordEffect {
    Start,
    Finish {
        run: String,
    },
    Enqueue {
        target: Option<String>,
    },
    Cancel {
        target: String,
    },
    Usage {
        cached: i64,
        uncached: i64,
        total: i64,
        cost: f64,
    },
    // Intentionally exhaustive and distinct: no wildcard may drop a new fact variant.
    Abort,
    Step,
    Tool,
    Deferred,
    Frame,
    Retry,
}
impl Meta {
    pub fn from_mutation(mutation: &SessionMutation) -> Result<Self> {
        let effect = match mutation {
            SessionMutation::Entry {
                seq,
                timestamp,
                lane,
                entry,
            } => {
                ensure(
                    *seq == entry.seq() && *timestamp == entry.base().timestamp,
                    "inconsistent entry envelope",
                )?;
                // List every entry variant, including non-message commits.
                let message = match entry {
                    Entry::Message(_) => true,
                    Entry::ModelChange(_)
                    | Entry::ThinkingLevel(_)
                    | Entry::ActiveTools(_)
                    | Entry::Compaction(_)
                    | Entry::BranchSummary(_)
                    | Entry::Custom(_) => false,
                };
                Effect::Entry {
                    id: entry.id().into(),
                    lane: lane.clone(),
                    parent: entry.parent_id().map(str::to_owned),
                    message,
                }
            }
            SessionMutation::Record { record } => {
                let effect = match record {
                    LaneRecord::OperationStarted(_) => RecordEffect::Start,
                    LaneRecord::OperationFinished(r) => RecordEffect::Finish {
                        run: r.run_id.clone(),
                    },
                    LaneRecord::QueueEnqueued(r) => RecordEffect::Enqueue {
                        target: r
                            .target
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                    },
                    LaneRecord::QueueCancelled(r) => RecordEffect::Cancel {
                        target: r.entry_id.clone(),
                    },
                    LaneRecord::Usage(r) => RecordEffect::Usage {
                        cached: r.usage.cache_read,
                        uncached: r
                            .usage
                            .input
                            .checked_add(r.usage.cache_write)
                            .ok_or("usage overflow")?,
                        total: r.usage.total_tokens,
                        cost: r.usage.cost.total,
                    },
                    LaneRecord::AbortRequested(_) => RecordEffect::Abort,
                    LaneRecord::StepAttempt(_) => RecordEffect::Step,
                    LaneRecord::ToolStarted(_) => RecordEffect::Tool,
                    LaneRecord::WriteDeferred(_) => RecordEffect::Deferred,
                    LaneRecord::AssistantFrame(_) => RecordEffect::Frame,
                    LaneRecord::RetryPending(_) => RecordEffect::Retry,
                };
                Effect::Record {
                    id: record.id().into(),
                    lane: record.lane().into(),
                    record: effect,
                }
            }
            SessionMutation::Lane { lane, leaf_id, .. } => Effect::Lane {
                lane: lane.clone(),
                leaf: leaf_id.clone(),
            },
            SessionMutation::FactName { .. } => Effect::Name,
            SessionMutation::FactLabel { target_id, .. } => Effect::Label {
                target: target_id.clone(),
            },
        };
        Ok(Self {
            seq: mutation.seq(),
            selection: super::selection::Selection::of(mutation),
            effect,
        })
    }
    pub(super) fn id(&self) -> Option<&str> {
        match &self.effect {
            Effect::Entry { id, .. } | Effect::Record { id, .. } => Some(id),
            _ => None,
        }
    }
    pub(super) fn lane(&self) -> Option<&str> {
        match &self.effect {
            Effect::Entry { lane, .. } => lane.as_deref(),
            Effect::Record { lane, .. } | Effect::Lane { lane, .. } => Some(lane),
            _ => None,
        }
    }
    pub(super) fn kind(&self) -> &'static str {
        match &self.effect {
            Effect::Entry { .. } => "entry",
            Effect::Record {
                record: RecordEffect::Cancel { .. },
                ..
            } => "cancel",
            Effect::Record {
                record: RecordEffect::Finish { .. },
                ..
            } => "finish",
            Effect::Record { .. } => "record",
            Effect::Lane { .. } => "lane",
            Effect::Name => "name",
            Effect::Label { .. } => "label",
        }
    }
    pub(super) fn target(&self) -> Option<&str> {
        match &self.effect {
            Effect::Label { target }
            | Effect::Record {
                record: RecordEffect::Cancel { target },
                ..
            } => Some(target),
            Effect::Record {
                record: RecordEffect::Finish { run },
                ..
            } => Some(run),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub seq: u64,
    pub id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub reference: Reference,
    pub target: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub seq: u64,
    pub heads: BTreeMap<String, Option<String>>,
    pub open: BTreeMap<String, Vec<Reference>>,
    pub pending: BTreeMap<String, Vec<Pending>>,
    pub name: Option<u64>,
    pub stats: SessionStats,
}
impl Default for Projection {
    fn default() -> Self {
        Self {
            seq: 0,
            heads: BTreeMap::from([("main".into(), None)]),
            open: BTreeMap::new(),
            pending: BTreeMap::new(),
            name: None,
            stats: SessionStats::default(),
        }
    }
}
impl Projection {
    pub(super) fn apply(&mut self, meta: &Meta, db: &Connection) -> Result<()> {
        ensure(
            self.seq.checked_add(1) == Some(meta.seq),
            "nonconsecutive native metadata",
        )?;
        if let Some(id) = meta.id() {
            ensure(!exists_id(db, id, self.seq)?, "duplicate native identity")?;
        }
        match &meta.effect {
            Effect::Entry {
                id,
                lane,
                parent,
                message,
            } => {
                if let Some(lane) = lane {
                    ensure(
                        self.heads.get(lane) == Some(parent),
                        "missing lane or nonchaining entry",
                    )?;
                }
                if let Some(parent) = parent {
                    ensure(exists_entry(db, parent, self.seq)?, "missing native parent")?;
                }
                if let Some(lane) = lane {
                    self.heads.insert(lane.clone(), Some(id.clone()));
                }
                for pending in self.pending.values_mut() {
                    pending.retain(|p| p.target != *id);
                }
                if *message {
                    self.stats.message_count = self
                        .stats
                        .message_count
                        .checked_add(1)
                        .ok_or("message count overflow")?;
                }
            }
            Effect::Record { id, lane, record } => {
                ensure(self.heads.contains_key(lane), "missing record lane")?;
                let reference = Reference {
                    seq: meta.seq,
                    id: id.clone(),
                };
                match record {
                    RecordEffect::Start => {
                        self.open.entry(lane.clone()).or_default().push(reference)
                    }
                    RecordEffect::Finish { run } => {
                        if let Some(open) = self.open.get_mut(lane) {
                            open.retain(|r| r.id != *run);
                        }
                    }
                    RecordEffect::Enqueue {
                        target: Some(target),
                    } => {
                        if !exists_entry(db, target, self.seq)?
                            && !cancelled(db, lane, target, self.seq)?
                        {
                            self.pending.entry(lane.clone()).or_default().push(Pending {
                                reference,
                                target: target.clone(),
                            });
                        }
                    }
                    RecordEffect::Cancel { target } => {
                        if let Some(pending) = self.pending.get_mut(lane) {
                            pending.retain(|p| p.target != *target);
                        }
                    }
                    RecordEffect::Usage {
                        cached,
                        uncached,
                        total,
                        cost,
                    } => {
                        self.stats.cached_tokens = self
                            .stats
                            .cached_tokens
                            .checked_add(*cached)
                            .ok_or("usage overflow")?;
                        self.stats.uncached_tokens = self
                            .stats
                            .uncached_tokens
                            .checked_add(*uncached)
                            .ok_or("usage overflow")?;
                        self.stats.total_tokens = self
                            .stats
                            .total_tokens
                            .checked_add(*total)
                            .ok_or("usage overflow")?;
                        self.stats.cost_total += cost;
                        ensure(self.stats.cost_total.is_finite(), "nonfinite usage")?;
                    }
                    RecordEffect::Enqueue { target: None }
                    | RecordEffect::Abort
                    | RecordEffect::Step
                    | RecordEffect::Tool
                    | RecordEffect::Deferred
                    | RecordEffect::Frame
                    | RecordEffect::Retry => {}
                }
            }
            Effect::Lane { lane, leaf } => {
                if let Some(leaf) = leaf {
                    ensure(
                        exists_entry(db, leaf, self.seq)?,
                        "missing native lane target",
                    )?;
                }
                self.heads.insert(lane.clone(), leaf.clone());
            }
            Effect::Name => self.name = Some(meta.seq),
            Effect::Label { target } => ensure(
                exists_entry(db, target, self.seq)?,
                "missing native label target",
            )?,
        }
        self.open.retain(|_, r| !r.is_empty());
        self.pending.retain(|_, r| !r.is_empty());
        self.seq = meta.seq;
        Ok(())
    }
}
fn exists_id(db: &Connection, id: &str, at: u64) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM facts WHERE id=?1 AND seq<=?2)",
        params![id, at],
        |r| r.get(0),
    )?)
}
pub(super) fn exists_entry(db: &Connection, id: &str, at: u64) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM facts WHERE id=?1 AND category='entry' AND seq<=?2)",
        params![id, at],
        |r| r.get(0),
    )?)
}
pub(super) fn cancelled(db: &Connection, lane: &str, target: &str, at: u64) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM facts WHERE lane=?1 AND category='cancel' AND target=?2 AND seq<=?3)", params![lane, target, at], |r| r.get(0))?)
}
pub(super) fn digest(identity: &Identity, seq: u64, domain: &str, bytes: &[u8]) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(&(FORMAT, identity, seq, domain)).expect("binding serializes"));
    hash.update(bytes);
    hash.finalize().to_vec()
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    format: String,
    identity: Identity,
    projection: Projection,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reads {
    pub checkpoint_bytes: usize,
    pub tail_rows: usize,
    pub tail_bytes: usize,
    pub reference_rows: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    BeforeCommit,
    LostAck,
}
pub(super) fn insert_fact(
    db: &Connection,
    identity: &Identity,
    state: &mut Projection,
    mutation: &SessionMutation,
) -> Result<()> {
    let meta = Meta::from_mutation(mutation)?;
    ensure(meta.seq <= i64::MAX as u64, "native sequence overflow")?;
    let json = serde_json::to_string(&meta)?;
    let body = encode_mutation(mutation);
    ensure(
        json.len() <= META_LIMIT && body.len() <= BODY_LIMIT,
        "native fact budget exceeded",
    )?;
    ensure(
        super::codec::parse(body.trim_end_matches('\n'))? == *mutation,
        "native codec roundtrip mismatch",
    )?;
    state.apply(&meta, db)?;
    db.execute(
        "INSERT INTO facts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            meta.seq,
            meta.id(),
            meta.lane(),
            meta.kind(),
            meta.target(),
            json,
            digest(identity, meta.seq, "metadata", json.as_bytes()),
            body,
            digest(identity, meta.seq, "body", body.as_bytes())
        ],
    )?;
    super::index::publish(db, identity, &meta, mutation)?;
    Ok(())
}
#[cfg(test)]
type ProcessProbe = std::sync::Arc<dyn Fn(&str) + Send + Sync>;
pub struct Store {
    pub(super) connection: Connection,
    pub(super) identity: Identity,
    // Explicit connection-local test observer. Never present in a product artifact,
    // and deliberately not inferred from environment/global mutable state.
    #[cfg(test)]
    pub(super) process_probe: Option<ProcessProbe>,
}
impl Store {
    pub fn create(path: &Path, identity: Identity) -> Result<Self> {
        Self::create_initialized(path, identity, |_, _| Ok(()))
    }
    pub(super) fn create_initialized(
        path: &Path,
        identity: Identity,
        initialize: impl FnOnce(&Connection, &Identity) -> Result<()>,
    ) -> Result<Self> {
        ensure(
            !identity.session.is_empty()
                && !identity.generation.is_empty()
                && identity.session.len() <= 128
                && identity.generation.len() <= 128,
            "invalid native identity",
        )?;
        drop(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(ReservationError)?,
        );
        let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(&format!("PRAGMA application_id={APP}; PRAGMA user_version=3;
            CREATE TABLE root(singleton INTEGER PRIMARY KEY CHECK(singleton=1),format TEXT NOT NULL,session TEXT NOT NULL,generation TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>=0));
            CREATE TABLE facts(seq INTEGER PRIMARY KEY CHECK(seq>0),id TEXT UNIQUE,lane TEXT,category TEXT NOT NULL,target TEXT,metadata TEXT NOT NULL,metadata_digest BLOB NOT NULL,body TEXT NOT NULL,body_digest BLOB NOT NULL);
            CREATE INDEX lane_kind_target ON facts(lane,category,target,seq);
            CREATE INDEX kind_target_seq ON facts(category,target,seq);
            CREATE TABLE checkpoint(singleton INTEGER PRIMARY KEY CHECK(singleton=1),json TEXT NOT NULL,digest BLOB NOT NULL);
            CREATE TABLE auxiliary(key TEXT PRIMARY KEY,json TEXT NOT NULL,digest BLOB NOT NULL);"))?;
        super::index::schema(&tx)?;
        tx.execute(
            "INSERT INTO root VALUES(1,?1,?2,?3,0)",
            params![FORMAT, identity.session, identity.generation],
        )?;
        publish(&tx, &identity, &Projection::default())?;
        initialize(&tx, &identity)?;
        tx.commit()?;
        Ok(Self {
            connection,
            identity,
            #[cfg(test)]
            process_probe: None,
        })
    }
    pub fn open(path: &Path, identity: Identity) -> Result<Self> {
        validate_header(path)?;
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        ensure(
            connection.query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))? == APP
                && connection.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? == 3,
            "unsupported native fact format",
        )?;
        let mut store = Self {
            connection,
            identity,
            #[cfg(test)]
            process_probe: None,
        };
        store.restore()?;
        Ok(store)
    }
    pub fn restore(&mut self) -> Result<(Projection, Reads)> {
        let tx = self.connection.transaction()?;
        let result = restore(&tx, &self.identity)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn append(
        &mut self,
        mutation: &SessionMutation,
        expected: Option<u64>,
        fault: Fault,
    ) -> Result<()> {
        let meta = Meta::from_mutation(mutation)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut state, _) = restore(&tx, &self.identity)?;
        ensure(
            expected.is_none_or(|seq| seq == state.seq),
            "stale native revision",
        )?;
        let checkpoint_seq = read_checkpoint(&tx, &self.identity)?.0.seq;
        insert_fact(&tx, &self.identity, &mut state, mutation)?;
        tx.execute("UPDATE root SET revision=?1 WHERE singleton=1", [meta.seq])?;
        if meta.seq - checkpoint_seq >= TAIL {
            publish(&tx, &self.identity, &state)?;
        }
        ensure(
            fault != Fault::BeforeCommit,
            "injected native precommit failure",
        )?;
        tx.commit()?;
        ensure(fault != Fault::LostAck, "native append outcome uncertain")?;
        Ok(())
    }
    pub fn mutation(&self, seq: u64) -> Result<SessionMutation> {
        read_mutation(&self.connection, &self.identity, seq)
    }
    pub fn mutation_id(&self, id: &str) -> Result<Option<SessionMutation>> {
        let seq = self
            .connection
            .query_row("SELECT seq FROM facts WHERE id=?1", [id], |r| {
                r.get::<_, u64>(0)
            })
            .optional()?;
        seq.map(|seq| self.mutation(seq)).transpose()
    }
    pub fn reconcile(&self, request: &SessionMutation) -> Result<bool> {
        // Same identity is insufficient: compare exact requested canonical mutation.
        let meta = Meta::from_mutation(request)?;
        let stored = if let Some(id) = meta.id() {
            self.mutation_id(id)?
        } else {
            let present: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM facts WHERE seq=?1)",
                [meta.seq],
                |r| r.get(0),
            )?;
            if present {
                Some(self.mutation(meta.seq)?)
            } else {
                None
            }
        };
        match stored {
            Some(stored) => {
                ensure(stored == *request, "native reconcile content mismatch")?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
    pub fn pending(&mut self, lane: &str) -> Result<Vec<LaneRecord>> {
        let tx = self.connection.transaction()?;
        let (state, _) = restore(&tx, &self.identity)?;
        let mut result = Vec::new();
        for pending in state.pending.get(lane).into_iter().flatten() {
            match read_mutation(&tx, &self.identity, pending.reference.seq)? {
                SessionMutation::Record {
                    record: record @ LaneRecord::QueueEnqueued(_),
                } => result.push(record),
                _ => return Err("invalid pending body".into()),
            }
        }
        tx.commit()?;
        Ok(result)
    }
    pub fn open_operations(&mut self, lane: &str) -> Result<Vec<OperationStartedRecord>> {
        let tx = self.connection.transaction()?;
        let (state, _) = restore(&tx, &self.identity)?;
        let mut result = Vec::new();
        for reference in state.open.get(lane).into_iter().flatten().rev() {
            match read_mutation(&tx, &self.identity, reference.seq)? {
                SessionMutation::Record {
                    record: LaneRecord::OperationStarted(record),
                } => result.push(record),
                _ => return Err("invalid operation body".into()),
            }
        }
        tx.commit()?;
        Ok(result)
    }
    pub fn name(&mut self) -> Result<Option<String>> {
        let tx = self.connection.transaction()?;
        let (state, _) = restore(&tx, &self.identity)?;
        let result = match state.name {
            Some(seq) => match read_mutation(&tx, &self.identity, seq)? {
                SessionMutation::FactName { name, .. } => name,
                _ => return Err("invalid name body".into()),
            },
            None => None,
        };
        tx.commit()?;
        Ok(result)
    }
    pub fn label(&self, target: &str) -> Result<Option<String>> {
        let seq=self.connection.query_row("SELECT seq FROM facts WHERE category='label' AND target=?1 ORDER BY seq DESC LIMIT 1",[target],|r|r.get::<_,u64>(0)).optional()?;
        match seq {
            Some(seq) => match self.mutation(seq)? {
                SessionMutation::FactLabel { label, .. } => Ok(label),
                _ => Err("invalid label body".into()),
            },
            None => Ok(None),
        }
    }
    pub fn forbid_bodies(&self, forbidden: bool) {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        self.connection
            .authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Read {
                    table_name: "facts",
                    column_name: "body",
                } if forbidden => Authorization::Deny,
                _ => Authorization::Allow,
            }));
    }
}
pub(super) fn publish(db: &Connection, identity: &Identity, state: &Projection) -> Result<()> {
    let json = serde_json::to_string(&Checkpoint {
        format: FORMAT.into(),
        identity: identity.clone(),
        projection: state.clone(),
    })?;
    ensure(
        json.len() <= CHECKPOINT_LIMIT,
        "native active state budget exceeded",
    )?;
    db.execute("INSERT INTO checkpoint VALUES(1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET json=excluded.json,digest=excluded.digest",params![json,digest(identity,0,"checkpoint",json.as_bytes())])?;
    Ok(())
}
pub(super) fn read_checkpoint(db: &Connection, identity: &Identity) -> Result<(Projection, usize)> {
    let (length,json,hash):(usize,Option<String>,Option<Vec<u8>>)=db.query_row("SELECT length(CAST(json AS BLOB)),CASE WHEN length(CAST(json AS BLOB))<=?1 THEN json END,CASE WHEN length(digest)=32 THEN digest END FROM checkpoint WHERE singleton=1",[CHECKPOINT_LIMIT],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    ensure(
        length <= CHECKPOINT_LIMIT,
        "native checkpoint budget exceeded",
    )?;
    let json = json.ok_or("missing native checkpoint")?;
    ensure(
        hash.as_deref() == Some(digest(identity, 0, "checkpoint", json.as_bytes()).as_slice()),
        "native checkpoint digest mismatch",
    )?;
    let checkpoint: Checkpoint = serde_json::from_str(&json)?;
    ensure(
        checkpoint.format == FORMAT && checkpoint.identity == *identity,
        "native checkpoint identity mismatch",
    )?;
    Ok((checkpoint.projection, length))
}
pub(super) fn read_metadata(
    db: &Connection,
    identity: &Identity,
    seq: u64,
) -> Result<(Meta, usize)> {
    struct Row {
        json: Option<String>,
        hash: Option<Vec<u8>>,
        id: Option<String>,
        lane: Option<String>,
        category: Option<String>,
        target: Option<String>,
        budget_ok: bool,
    }
    let row = db.query_row(
        "SELECT CASE WHEN length(CAST(metadata AS BLOB))<=?2 THEN metadata END,
         CASE WHEN length(metadata_digest)=32 THEN metadata_digest END,
         CASE WHEN length(CAST(id AS BLOB))<=?2 THEN id END,
         CASE WHEN length(CAST(lane AS BLOB))<=?2 THEN lane END,
         CASE WHEN length(CAST(category AS BLOB))<=?2 THEN category END,
         CASE WHEN length(CAST(target AS BLOB))<=?2 THEN target END,
         COALESCE(length(CAST(id AS BLOB)),0)<=?2 AND COALESCE(length(CAST(lane AS BLOB)),0)<=?2
         AND length(CAST(category AS BLOB))<=?2 AND COALESCE(length(CAST(target AS BLOB)),0)<=?2
         FROM facts WHERE seq=?1",
        params![seq, META_LIMIT],
        |r| {
            Ok(Row {
                json: r.get(0)?,
                hash: r.get(1)?,
                id: r.get(2)?,
                lane: r.get(3)?,
                category: r.get(4)?,
                target: r.get(5)?,
                budget_ok: r.get(6)?,
            })
        },
    )?;
    ensure(row.budget_ok, "native metadata index budget exceeded")?;
    let Row {
        json,
        hash,
        id,
        lane,
        category,
        target,
        ..
    } = row;
    let json = json.ok_or("native metadata budget exceeded")?;
    ensure(
        hash.as_deref() == Some(digest(identity, seq, "metadata", json.as_bytes()).as_slice()),
        "native metadata digest mismatch",
    )?;
    let meta: Meta = serde_json::from_str(&json)?;
    ensure(
        meta.seq == seq
            && meta.id() == id.as_deref()
            && meta.lane() == lane.as_deref()
            && Some(meta.kind()) == category.as_deref()
            && meta.target() == target.as_deref(),
        "native metadata index mismatch",
    )?;
    super::index::validate_selection(db, &meta)?;
    Ok((meta, json.len()))
}
pub(super) fn read_mutation(
    db: &Connection,
    identity: &Identity,
    seq: u64,
) -> Result<SessionMutation> {
    let (body,hash):(Option<String>,Option<Vec<u8>>)=db.query_row("SELECT CASE WHEN length(CAST(body AS BLOB))<=?2 THEN body END,CASE WHEN length(body_digest)=32 THEN body_digest END FROM facts WHERE seq=?1",params![seq,BODY_LIMIT],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let body = body.ok_or("native body budget exceeded")?;
    ensure(
        hash.as_deref() == Some(digest(identity, seq, "body", body.as_bytes()).as_slice()),
        "native body digest mismatch",
    )?;
    let mutation = super::codec::parse(body.trim_end_matches('\n'))?;
    let meta = read_metadata(db, identity, seq)?.0;
    ensure(
        Meta::from_mutation(&mutation)? == meta,
        "native body metadata mismatch",
    )?;
    super::index::verify_payload(db, identity, &meta, &mutation)?;
    Ok(mutation)
}
fn validate_references(db: &Connection, identity: &Identity, state: &Projection) -> Result<usize> {
    ensure(
        state.heads.contains_key("main") && state.stats.cost_total.is_finite(),
        "invalid native projection",
    )?;
    let mut reads = 0;
    for head in state.heads.values().flatten() {
        let seq: u64 = db.query_row(
            "SELECT seq FROM facts WHERE id=?1 AND category='entry' AND seq<=?2",
            params![head, state.seq],
            |r| r.get(0),
        )?;
        ensure(
            matches!(read_metadata(db,identity,seq)?.0.effect,Effect::Entry{id,..} if id==*head),
            "invalid native head reference",
        )?;
        reads += 1;
    }
    let mut seen = BTreeSet::new();
    for (lane, open) in &state.open {
        ensure(
            state.heads.contains_key(lane) && !open.is_empty(),
            "invalid open reference lane",
        )?;
        let mut previous = 0;
        for reference in open {
            ensure(
                reference.seq > previous
                    && reference.seq <= state.seq
                    && seen.insert(reference.seq),
                "invalid open reference order",
            )?;
            previous = reference.seq;
            ensure(
                matches!(read_metadata(db,identity,reference.seq)?.0.effect,Effect::Record{id,lane:actual,record:RecordEffect::Start} if id==reference.id && actual==*lane),
                "invalid open reference",
            )?;
            let finished:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM facts WHERE lane=?1 AND category='finish' AND target=?2 AND seq>?3 AND seq<=?4)",params![lane,reference.id,reference.seq,state.seq],|r|r.get(0))?;
            ensure(!finished, "terminal native operation referenced as open")?;
            reads += 1;
        }
    }
    for (lane, pending) in &state.pending {
        ensure(
            state.heads.contains_key(lane) && !pending.is_empty(),
            "invalid pending reference lane",
        )?;
        let mut previous = 0;
        for pending in pending {
            let reference = &pending.reference;
            ensure(
                reference.seq > previous
                    && reference.seq <= state.seq
                    && seen.insert(reference.seq),
                "invalid pending reference order",
            )?;
            previous = reference.seq;
            ensure(
                matches!(read_metadata(db,identity,reference.seq)?.0.effect,Effect::Record{id,lane:actual,record:RecordEffect::Enqueue{target:Some(target)}} if id==reference.id && actual==*lane && target==pending.target),
                "invalid pending reference",
            )?;
            ensure(
                !exists_entry(db, &pending.target, state.seq)?
                    && !cancelled(db, lane, &pending.target, state.seq)?,
                "retired native enqueue referenced as pending",
            )?;
            reads += 1;
        }
    }
    if let Some(seq) = state.name {
        ensure(
            seq > 0
                && seq <= state.seq
                && matches!(read_metadata(db, identity, seq)?.0.effect, Effect::Name),
            "invalid native name reference",
        )?;
        reads += 1;
    }
    Ok(reads)
}
pub(super) fn restore(db: &Connection, identity: &Identity) -> Result<(Projection, Reads)> {
    let (format,session,generation,revision):(String,String,String,u64)=db.query_row("SELECT CASE WHEN length(CAST(format AS BLOB))<=128 THEN format ELSE '' END,CASE WHEN length(CAST(session AS BLOB))<=128 THEN session ELSE '' END,CASE WHEN length(CAST(generation AS BLOB))<=128 THEN generation ELSE '' END,revision FROM root WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure(
        format == FORMAT && session == identity.session && generation == identity.generation,
        "native root identity mismatch",
    )?;
    let (mut state, checkpoint_bytes) = read_checkpoint(db, identity)?;
    ensure(
        state.seq <= revision && revision - state.seq <= TAIL,
        "native tail budget exceeded",
    )?;
    let max: u64 = db.query_row("SELECT COALESCE(MAX(seq),0) FROM facts", [], |r| r.get(0))?;
    ensure(max == revision, "native history revision mismatch")?;
    let reference_rows = validate_references(db, identity, &state)?;
    let mut reads = Reads {
        checkpoint_bytes,
        reference_rows,
        ..Reads::default()
    };
    // Deliberately only sequence keys, followed by bounded point metadata reads.
    let mut statement = db.prepare("SELECT seq FROM facts WHERE seq>?1 ORDER BY seq LIMIT ?2")?;
    let rows = statement.query_map(params![state.seq, TAIL + 1], |r| r.get::<_, u64>(0))?;
    for row in rows {
        ensure(
            reads.tail_rows < TAIL as usize,
            "native tail budget exceeded",
        )?;
        let (meta, bytes) = read_metadata(db, identity, row?)?;
        state.apply(&meta, db)?;
        reads.tail_rows += 1;
        reads.tail_bytes += bytes;
    }
    ensure(state.seq == revision, "native tail missing")?;
    Ok((state, reads))
}
