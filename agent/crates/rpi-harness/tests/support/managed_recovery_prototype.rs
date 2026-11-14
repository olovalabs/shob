//! Test-only protocol experiment. Not a SessionStorage or a migration utility.
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs::OpenOptions as FsOptions, path::Path, time::Duration};

pub const FORMAT: &str = "rpi-managed-recovery-prototype-v1";
pub const MAX_TAIL: u64 = 16;
const APPLICATION_ID: i64 = 0x5250_4950;
const MAX_METADATA: usize = 16 * 1024;
const MAX_CHECKPOINT: usize = 128 * 1024;
const MAX_BODY: usize = 256 * 1024;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn require(condition: bool, reason: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(reason.into())
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub session: String,
    pub generation: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub id: String,
    pub lane: String,
    pub action: Action,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Action {
    CreateLane { at: Option<String> },
    MoveLane { to: Option<String> },
    Message { parent: Option<String> },
    Enqueue { target: String, queue: String },
    Cancel { target: String },
    Start,
    Finish { run: String },
}
impl Event {
    fn kind(&self) -> &'static str {
        match self.action {
            Action::CreateLane { .. } => "lane_create",
            Action::MoveLane { .. } => "lane_move",
            Action::Message { .. } => "message",
            Action::Enqueue { .. } => "enqueue",
            Action::Cancel { .. } => "cancel",
            Action::Start => "start",
            Action::Finish { .. } => "finish",
        }
    }
    fn target(&self) -> Option<&str> {
        match &self.action {
            Action::Message { .. } => Some(&self.id),
            Action::Enqueue { target, .. } | Action::Cancel { target } => Some(target),
            Action::Finish { run } => Some(run),
            _ => None,
        }
    }
    fn validate(&self) -> Result<()> {
        require(
            valid_id(&self.id) && valid_id(&self.lane),
            "invalid event identity",
        )?;
        let reference = match &self.action {
            Action::CreateLane { at } => at.as_deref(),
            Action::MoveLane { to } => to.as_deref(),
            Action::Message { parent } => parent.as_deref(),
            _ => self.target(),
        };
        require(reference.is_none_or(valid_id), "invalid event reference")?;
        if let Action::Enqueue { queue, .. } = &self.action {
            require(
                matches!(queue.as_str(), "steering" | "follow_up"),
                "invalid queue kind",
            )?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub enqueue: String,
    pub sequence: u64,
    pub queue: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub sequence: u64,
    pub heads: BTreeMap<String, Option<String>>,
    pub pending: BTreeMap<String, BTreeMap<String, Pending>>,
    pub operations: BTreeMap<String, Vec<String>>,
}
impl Default for Recovery {
    fn default() -> Self {
        Self {
            sequence: 0,
            heads: BTreeMap::from([("main".into(), None)]),
            pending: BTreeMap::new(),
            operations: BTreeMap::new(),
        }
    }
}
impl Recovery {
    fn validate(&self) -> Result<()> {
        require(
            self.heads.contains_key("main"),
            "checkpoint lacks main lane",
        )?;
        for (lane, head) in &self.heads {
            require(
                valid_id(lane) && head.as_deref().is_none_or(valid_id),
                "invalid checkpoint head",
            )?;
        }
        for (lane, pending) in &self.pending {
            require(
                self.heads.contains_key(lane) && !pending.is_empty(),
                "invalid checkpoint pending lane",
            )?;
            for (target, item) in pending {
                require(
                    valid_id(target)
                        && valid_id(&item.enqueue)
                        && item.sequence > 0
                        && item.sequence <= self.sequence
                        && matches!(item.queue.as_str(), "steering" | "follow_up"),
                    "invalid checkpoint pending reference",
                )?;
            }
        }
        for (lane, runs) in &self.operations {
            require(
                self.heads.contains_key(lane) && !runs.is_empty(),
                "invalid checkpoint operation lane",
            )?;
            let mut seen = std::collections::HashSet::new();
            require(
                runs.iter().all(|run| valid_id(run) && seen.insert(run)),
                "invalid checkpoint operation reference",
            )?;
        }
        Ok(())
    }
    pub fn apply(
        &mut self,
        event: &Event,
        sequence: u64,
        entry_exists: impl Fn(&str) -> Result<bool>,
    ) -> Result<()> {
        event.validate()?;
        require(
            self.sequence.checked_add(1) == Some(sequence),
            "nonconsecutive recovery sequence",
        )?;
        if !matches!(event.action, Action::CreateLane { .. }) {
            require(
                self.heads.contains_key(&event.lane),
                "missing recovery lane",
            )?;
        }
        match &event.action {
            Action::CreateLane { at } => {
                require(
                    !self.heads.contains_key(&event.lane),
                    "duplicate recovery lane",
                )?;
                if let Some(id) = at {
                    require(entry_exists(id)?, "missing lane entry")?;
                }
                self.heads.insert(event.lane.clone(), at.clone());
            }
            Action::MoveLane { to } => {
                if let Some(id) = to {
                    require(entry_exists(id)?, "missing lane entry")?;
                }
                self.heads.insert(event.lane.clone(), to.clone());
            }
            Action::Message { parent } => {
                require(
                    self.heads.get(&event.lane) == Some(parent),
                    "message does not extend lane head",
                )?;
                self.heads
                    .insert(event.lane.clone(), Some(event.id.clone()));
                for pending in self.pending.values_mut() {
                    pending.remove(&event.id);
                }
            }
            Action::Enqueue { target, queue } => {
                if !entry_exists(target)? {
                    self.pending.entry(event.lane.clone()).or_default().insert(
                        target.clone(),
                        Pending {
                            enqueue: event.id.clone(),
                            sequence,
                            queue: queue.clone(),
                        },
                    );
                }
            }
            Action::Cancel { target } => {
                if let Some(pending) = self.pending.get_mut(&event.lane) {
                    pending.remove(target);
                }
            }
            Action::Start => {
                self.operations
                    .entry(event.lane.clone())
                    .or_default()
                    .push(event.id.clone());
            }
            Action::Finish { run } => {
                let runs = self
                    .operations
                    .get_mut(&event.lane)
                    .ok_or("missing operation")?;
                require(runs.contains(run), "missing operation")?;
                runs.retain(|id| id != run);
            }
        }
        self.pending.retain(|_, values| !values.is_empty());
        self.operations.retain(|_, values| !values.is_empty());
        self.sequence = sequence;
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    format: String,
    identity: Identity,
    recovery: Recovery,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reads {
    pub checkpoint_bytes: usize,
    pub tail_rows: usize,
    pub tail_bytes: usize,
    pub decoded_body_bytes: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    AfterHistory,
    DuringCheckpoint,
    BeforeCommit,
    LostAck,
}

pub struct Store {
    connection: Connection,
    identity: Identity,
}
impl Store {
    pub fn create(path: &Path, identity: Identity) -> Result<Self> {
        require(
            valid_id(&identity.session) && valid_id(&identity.generation),
            "invalid store identity",
        )?;
        // O_EXCL/create_new, not exists-then-create. Never overwrite another artifact.
        drop(FsOptions::new().write(true).create_new(true).open(path)?);
        let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(&format!("PRAGMA application_id={APPLICATION_ID}; PRAGMA user_version=1;
            CREATE TABLE store_meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1), format TEXT NOT NULL, session TEXT NOT NULL, generation TEXT NOT NULL, revision INTEGER NOT NULL CHECK(revision>=0));
            CREATE TABLE history(seq INTEGER PRIMARY KEY CHECK(seq>0), id TEXT NOT NULL UNIQUE, lane TEXT NOT NULL, kind TEXT NOT NULL, target TEXT, event_json TEXT NOT NULL, event_digest BLOB NOT NULL, body BLOB NOT NULL, body_digest BLOB NOT NULL);
            CREATE INDEX history_lane_type_seq ON history(lane,kind,seq);
            CREATE INDEX history_target_type_seq ON history(target,kind,seq);
            CREATE UNIQUE INDEX enqueue_once ON history(lane,target) WHERE kind='enqueue';
            CREATE TABLE checkpoint(singleton INTEGER PRIMARY KEY CHECK(singleton=1), state_json TEXT NOT NULL, digest BLOB NOT NULL);"))?;
        tx.execute(
            "INSERT INTO store_meta VALUES(1,?1,?2,?3,0)",
            params![FORMAT, identity.session, identity.generation],
        )?;
        publish_checkpoint(&tx, &identity, &Recovery::default())?;
        tx.commit()?;
        Ok(Self {
            connection,
            identity,
        })
    }
    pub fn open(path: &Path, identity: Identity) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        require(
            connection.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))?
                == APPLICATION_ID
                && connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?
                    == 1,
            "unsupported managed store version",
        )?;
        let mut store = Self {
            connection,
            identity,
        };
        store.restore()?;
        Ok(store)
    }
    pub fn restore(&mut self) -> Result<(Recovery, Reads)> {
        let tx = self.connection.transaction()?;
        let result = restore_on(&tx, &self.identity)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn append(
        &mut self,
        event: Event,
        body: &[u8],
        expected: Option<u64>,
        fault: Fault,
    ) -> Result<u64> {
        event.validate()?;
        require(body.len() <= MAX_BODY, "body exceeds prototype limit")?;
        require(
            matches!(
                event.action,
                Action::Message { .. } | Action::Enqueue { .. }
            ) || body.is_empty(),
            "metadata event cannot carry a body",
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut state, _) = restore_on(&tx, &self.identity)?;
        require(
            expected.is_none_or(|revision| revision == state.sequence),
            "stale expected revision",
        )?;
        let sequence = state
            .sequence
            .checked_add(1)
            .filter(|seq| *seq <= i64::MAX as u64)
            .ok_or("history sequence overflow")?;
        let json = serde_json::to_string(&event)?;
        require(json.len() <= MAX_METADATA, "event exceeds metadata budget")?;
        state.apply(&event, sequence, |id| entry_exists(&tx, id, sequence - 1))?;
        tx.execute(
            "INSERT INTO history VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                sequence,
                event.id,
                event.lane,
                event.kind(),
                event.target(),
                json,
                metadata_digest(&self.identity, sequence, &json),
                body,
                body_digest(&self.identity, sequence, &event.id, body)
            ],
        )?;
        require(
            fault != Fault::AfterHistory,
            "injected failure after history",
        )?;
        tx.execute(
            "UPDATE store_meta SET revision=?1 WHERE singleton=1",
            [sequence],
        )?;
        let checkpoint_sequence = read_checkpoint(&tx, &self.identity)?.0.sequence;
        if sequence - checkpoint_sequence >= MAX_TAIL {
            publish_checkpoint(&tx, &self.identity, &state)?;
            require(
                fault != Fault::DuringCheckpoint,
                "injected failure during checkpoint",
            )?;
        }
        require(
            fault != Fault::BeforeCommit,
            "injected failure before commit",
        )?;
        tx.commit()?;
        require(
            fault != Fault::LostAck,
            "append outcome uncertain: reopen and reconcile mutation id",
        )?;
        Ok(sequence)
    }
    pub fn forbid_history_bodies(&self, forbidden: bool) {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        self.connection
            .authorizer(Some(move |context: AuthContext<'_>| match context.action {
                AuthAction::Read {
                    table_name: "history",
                    column_name: "body",
                } if forbidden => Authorization::Deny,
                _ => Authorization::Allow,
            }));
    }
    pub fn mutation_sequence(&self, id: &str) -> Result<Option<u64>> {
        Ok(self
            .connection
            .query_row("SELECT seq FROM history WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?)
    }
    pub fn body(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let row = self.connection.query_row("SELECT seq,length(body),CASE WHEN length(body)<=?2 THEN body END,body_digest FROM history WHERE id=?1", params![id, MAX_BODY], |row| Ok((row.get::<_, u64>(0)?, row.get::<_, usize>(1)?, row.get::<_, Option<Vec<u8>>>(2)?, row.get::<_, Vec<u8>>(3)?))).optional()?;
        row.map(|(sequence, length, bytes, digest)| {
            require(length <= MAX_BODY, "body exceeds prototype limit")?;
            let bytes = bytes.ok_or("missing body")?;
            require(
                digest == body_digest(&self.identity, sequence, id, &bytes),
                "history body digest mismatch",
            )?;
            Ok(bytes)
        })
        .transpose()
    }
    pub fn page(&self, lane: &str, after: u64, limit: usize) -> Result<Vec<(u64, String)>> {
        require((1..=128).contains(&limit), "invalid history page limit")?;
        let mut statement = self.connection.prepare("SELECT seq,id FROM history WHERE lane=?1 AND kind='message' AND seq>?2 ORDER BY seq LIMIT ?3")?;
        let result = statement
            .query_map(params![lane, after, limit], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(result)
    }
    pub fn count(&self) -> Result<u64> {
        Ok(self
            .connection
            .query_row("SELECT count(*) FROM history", [], |row| row.get(0))?)
    }
}
fn entry_exists(db: &Connection, id: &str, at: u64) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM history WHERE id=?1 AND kind='message' AND seq<=?2)",
        params![id, at],
        |row| row.get(0),
    )?)
}
fn metadata_digest(identity: &Identity, sequence: u64, json: &str) -> Vec<u8> {
    Sha256::digest(
        serde_json::to_vec(&(FORMAT, identity, sequence, json)).expect("metadata tuple serializes"),
    )
    .to_vec()
}
fn body_digest(identity: &Identity, sequence: u64, id: &str, body: &[u8]) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(
        serde_json::to_vec(&(FORMAT, identity, sequence, id)).expect("body binding serializes"),
    );
    digest.update(body);
    digest.finalize().to_vec()
}
fn publish_checkpoint(db: &Connection, identity: &Identity, state: &Recovery) -> Result<()> {
    state.validate()?;
    let json = serde_json::to_string(&Checkpoint {
        format: FORMAT.into(),
        identity: identity.clone(),
        recovery: state.clone(),
    })?;
    require(
        json.len() <= MAX_CHECKPOINT,
        "checkpoint exceeds active state budget",
    )?;
    db.execute("INSERT INTO checkpoint VALUES(1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET state_json=excluded.state_json,digest=excluded.digest", params![json, Sha256::digest(json.as_bytes()).to_vec()])?;
    Ok(())
}
fn read_checkpoint(db: &Connection, identity: &Identity) -> Result<(Recovery, usize)> {
    let (length, json, digest): (usize, Option<String>, Option<Vec<u8>>) = db.query_row("SELECT length(CAST(state_json AS BLOB)),CASE WHEN length(CAST(state_json AS BLOB))<=?1 THEN state_json END,CASE WHEN length(digest)=32 THEN digest END FROM checkpoint WHERE singleton=1", [MAX_CHECKPOINT], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    require(
        length <= MAX_CHECKPOINT,
        "checkpoint exceeds active state budget",
    )?;
    let json = json.ok_or("missing checkpoint")?;
    require(
        digest.as_deref() == Some(Sha256::digest(json.as_bytes()).as_slice()),
        "checkpoint digest mismatch",
    )?;
    let checkpoint: Checkpoint = serde_json::from_str(&json)?;
    require(
        checkpoint.format == FORMAT && &checkpoint.identity == identity,
        "checkpoint identity mismatch",
    )?;
    checkpoint.recovery.validate()?;
    Ok((checkpoint.recovery, length))
}
fn restore_on(db: &Connection, identity: &Identity) -> Result<(Recovery, Reads)> {
    let (format, session, generation, revision): (String, String, String, u64) = db.query_row(
        "SELECT CASE WHEN length(CAST(format AS BLOB))<=128 THEN format ELSE '' END,CASE WHEN length(CAST(session AS BLOB))<=128 THEN session ELSE '' END,CASE WHEN length(CAST(generation AS BLOB))<=128 THEN generation ELSE '' END,revision FROM store_meta WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    require(
        format == FORMAT && session == identity.session && generation == identity.generation,
        "store identity mismatch",
    )?;
    let (mut state, checkpoint_bytes) = read_checkpoint(db, identity)?;
    require(
        state.sequence <= revision && revision - state.sequence <= MAX_TAIL,
        "recovery tail exceeds budget",
    )?;
    let max_seq: u64 = db.query_row("SELECT COALESCE(MAX(seq),0) FROM history", [], |row| {
        row.get(0)
    })?;
    require(max_seq == revision, "history revision mismatch")?;
    let mut reads = Reads {
        checkpoint_bytes,
        ..Reads::default()
    };
    // Indexed range, bounded rows and bounded metadata; deliberately no body column.
    let mut statement = db.prepare("SELECT seq,length(CAST(event_json AS BLOB)),CASE WHEN length(CAST(event_json AS BLOB))<=?2 THEN event_json END,CASE WHEN length(event_digest)=32 THEN event_digest END FROM history WHERE seq>?1 ORDER BY seq LIMIT ?3")?;
    let mut rows = statement.query(params![state.sequence, MAX_METADATA, MAX_TAIL + 1])?;
    while let Some(row) = rows.next()? {
        let sequence: u64 = row.get(0)?;
        let length: usize = row.get(1)?;
        require(
            length <= MAX_METADATA && reads.tail_rows < MAX_TAIL as usize,
            "recovery metadata exceeds budget",
        )?;
        let json: String = row.get(2)?;
        let digest: Option<Vec<u8>> = row.get(3)?;
        require(
            digest.as_deref() == Some(metadata_digest(identity, sequence, &json).as_slice()),
            "tail metadata digest mismatch",
        )?;
        let event: Event = serde_json::from_str(&json)?;
        state.apply(&event, sequence, |id| entry_exists(db, id, sequence - 1))?;
        reads.tail_rows += 1;
        reads.tail_bytes += length;
    }
    require(state.sequence == revision, "missing recovery tail")?;
    Ok((state, reads))
}
