//! Native primary-entry archival bodies; no observer, replay, repair, or blob sidecar.
//!
//! Compact `serde_json::to_writer` serialization borrows the caller's value. The
//! synchronous sink stages at most 32 MiB of decoded chunk buffers (not an RSS
//! bound); encoding is preflighted before the first append, then repeated one
//! bounded chunk at a time. Metadata/indexes and branch walks grow with history;
//! memory/read-all backends and explicit history APIs may retain all payloads.
//! ACK means the existing backend durability contract, not a new fsync guarantee.
//! Errors/cancellation can leave orphan chunks or an uncertain commit; never retry
//! an append or re-execute a tool here. Forks resolve only destination-owned entries.
use std::io::{self, Write};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{SessionError, SessionResult};
use crate::session::{types::*, Session};

pub const CHUNK_CUSTOM_TYPE: &str = "rpi.tool_program.body_chunk.v1";
pub const COMMIT_CUSTOM_TYPE: &str = "rpi.tool_program.body_commit.v1";
pub const CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_CHUNKS: usize = 1024;
pub const MAX_BODY_BYTES: usize = CHUNK_BYTES * MAX_CHUNKS;
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;
pub const MAX_PAGE_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyKind {
    Requested,
    Prepared,
    Update,
    Final,
}

/// The terminal descriptor. `descriptorId` is generated natively, not a path.
/// Its presence is a commit, NOT proof that every chunk remains available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BodyDescriptor {
    pub descriptor_id: String,
    pub version: u8,
    pub body_id: String,
    pub child_id: String,
    pub kind: BodyKind,
    pub codec: String,
    pub encoding: String,
    pub byte_length: usize,
    pub chunk_bytes: usize,
    pub chunks: usize,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredChunk {
    body_id: String,
    child_id: String,
    index: usize,
    data: String,
    sha256: String,
}

/// Explicit source binding, not an authenticated token or authorization boundary.
/// Rebased/foreign contexts are rejected; callers may select an index only within
/// this validated body. No source-session fallback is performed after a fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BodyCursor {
    pub session_id: String,
    pub lane: String,
    pub leaf_id: String,
    pub descriptor_id: String,
    pub next_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BodyPageQuery {
    pub descriptor_id: String,
    pub leaf_id: Option<String>,
    pub cursor: Option<BodyCursor>,
    /// 1..=8. The decoded-byte cap can yield fewer chunks.
    pub limit: usize,
    /// 1..=65536 decoded bytes; an oversized first chunk is an error.
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BodyPageChunk {
    pub index: usize,
    pub byte_offset: usize,
    pub byte_length: usize,
    pub data: String,
    pub sha256: String,
}

/// Each returned chunk's checksum is verified. Whole-body integrity requires the
/// consumer to stream all decoded pages and compare `descriptor.sha256`.
/// There is deliberately no `complete`/`available` flag for unread chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BodyPage {
    pub descriptor: BodyDescriptor,
    pub leaf_id: String,
    pub byte_offset: usize,
    pub byte_length: usize,
    pub total_bytes: usize,
    pub chunks: Vec<BodyPageChunk>,
    pub next_cursor: Option<BodyCursor>,
}

fn invalid(message: &str) -> SessionError {
    SessionError::invalid_payload(message)
}
fn id(value: &str) -> SessionResult<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(invalid("invalid body source identity"));
    }
    Ok(())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn chunk_id(body: &str, index: usize) -> String {
    format!("tpbody-{body}-chunk-{index}")
}
fn commit_id(body: &str) -> String {
    format!("tpbody-{body}-commit")
}

struct ChunkSink {
    chunks: Vec<Vec<u8>>,
    bytes: usize,
    max: usize,
    hash: Sha256,
}
impl Write for ChunkSink {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let len = bytes.len();
        if len > self.max.saturating_sub(self.bytes) {
            return Err(io::Error::other("body exceeds admission quota"));
        }
        self.hash.update(bytes);
        self.bytes += len;
        while !bytes.is_empty() {
            if self.chunks.last().is_none_or(|c| c.len() == CHUNK_BYTES) {
                self.chunks.push(Vec::with_capacity(CHUNK_BYTES));
            }
            let chunk = self.chunks.last_mut().expect("chunk exists");
            let count = bytes.len().min(CHUNK_BYTES - chunk.len());
            chunk.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
        }
        Ok(len)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct ByteCounter(usize);
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encoded_size<T: Serialize>(value: &T) -> SessionResult<usize> {
    let mut count = ByteCounter(0);
    serde_json::to_writer(&mut count, value).map_err(|e| invalid(&e.to_string()))?;
    Ok(count.0)
}
fn provision<T: Serialize>(
    entry_id: String,
    custom_type: &str,
    data: &T,
) -> SessionResult<ProvisionedEntry> {
    // Only bounded DTOs become Values; never the original body.
    Ok(ProvisionedEntry {
        id: entry_id,
        kind: ProvisionedKind::Custom {
            custom_type: custom_type.into(),
            data: Some(serde_json::to_value(data).map_err(|e| invalid(&e.to_string()))?),
        },
    })
}
fn preflight(entry: &ProvisionedEntry) -> SessionResult<()> {
    let ProvisionedKind::Custom { custom_type, data } = &entry.kind else {
        unreachable!()
    };
    // Worst-case escaped bounded parent ID, integer widths and timestamp. CAS
    // below ensures the real parent was validated before this exact append.
    let envelope = Entry::Custom(CustomEntry {
        base: EntryBase {
            entry_type: "custom".into(),
            id: entry.id.clone(),
            seq: u64::MAX,
            parent_id: Some("\0".repeat(512)),
            timestamp: i64::MIN,
        },
        custom_type: custom_type.clone(),
        data: data.clone(),
    });
    if encoded_size(&envelope)? > MAX_ENTRY_BYTES {
        return Err(invalid("encoded body entry exceeds 64 KiB"));
    }
    Ok(())
}
async fn leaf(session: &Session, lane: &str) -> SessionResult<String> {
    id(lane)?;
    let leaf = session
        .get_lanes()
        .await?
        .into_iter()
        .find(|p| p.lane == lane)
        .ok_or_else(|| SessionError::invalid_lane("unknown body lane"))?
        .leaf_id
        .ok_or_else(|| invalid("body lane has no child entry"))?;
    id(&leaf)?;
    Ok(leaf)
}
async fn member(session: &Session, leaf: &str, entry_id: &str) -> SessionResult<Entry> {
    id(entry_id)?;
    let entries = session
        .storage()
        .find_entries_on_branch(
            &EntryQuery {
                entry_id: Some(entry_id.into()),
                limit: Some(1),
                ..Default::default()
            },
            &BranchBounds::default(),
            leaf,
        )
        .await?;
    if entries.len() != 1 || entries[0].id() != entry_id {
        return Err(SessionError::not_found(
            "body entry unavailable on selected branch",
        ));
    }
    Ok(entries.into_iter().next().expect("one entry"))
}
fn child(entry: &Entry) -> SessionResult<()> {
    match entry {
        Entry::Custom(c)
            if c.custom_type != CHUNK_CUSTOM_TYPE && c.custom_type != COMMIT_CUSTOM_TYPE =>
        {
            Ok(())
        }
        _ => Err(invalid(
            "child must reference a preexisting native custom entry",
        )),
    }
}
fn chunk(descriptor: &BodyDescriptor, index: usize, bytes: &[u8]) -> StoredChunk {
    StoredChunk {
        body_id: descriptor.body_id.clone(),
        child_id: descriptor.child_id.clone(),
        index,
        data: STANDARD.encode(bytes),
        sha256: digest(bytes),
    }
}

/// Admit canonical compact JSON of a borrowed value, then append chunks in order
/// and the descriptor LAST. Quota is 1..=32 MiB; arbitrary output is not accepted.
/// Optimistic conditional appends fail closed on concurrent history mutation;
/// there is no CAS/ACK retry. This also prevents a lane move between validation
/// and append from silently publishing a broken branch. Orphans are not repaired.
pub async fn append_body<T: Serialize + Sync>(
    session: &Session,
    lane: &str,
    child_id: &str,
    kind: BodyKind,
    value: &T,
    max_bytes: usize,
) -> SessionResult<BodyDescriptor> {
    // Coordinate native facade writes for the complete preflight/chunk/commit
    // transaction. CAS still detects uncoordinated storage/process mutations;
    // no uncertain append is retried. Never await tool execution under this gate.
    session
        .with_mutation(|session| async move {
            append_body_locked(&session, lane, child_id, kind, value, max_bytes).await
        })
        .await
}
async fn append_body_locked<T: Serialize + Sync>(
    session: &Session,
    lane: &str,
    child_id: &str,
    kind: BodyKind,
    value: &T,
    max_bytes: usize,
) -> SessionResult<BodyDescriptor> {
    if !(1..=MAX_BODY_BYTES).contains(&max_bytes) {
        return Err(invalid("body quota must be 1..=32 MiB"));
    }
    id(child_id)?;
    let anchor = leaf(session, lane).await?;
    child(&member(session, &anchor, child_id).await?)?;
    let mut sink = ChunkSink {
        chunks: vec![],
        bytes: 0,
        max: max_bytes,
        hash: Sha256::new(),
    };
    serde_json::to_writer(&mut sink, value).map_err(|e| invalid(&e.to_string()))?;
    let body_id = Uuid::new_v4().to_string();
    let descriptor = BodyDescriptor {
        descriptor_id: commit_id(&body_id),
        version: 1,
        body_id,
        child_id: child_id.into(),
        kind,
        codec: "native-json-v1".into(),
        encoding: "base64".into(),
        byte_length: sink.bytes,
        chunk_bytes: CHUNK_BYTES,
        chunks: sink.chunks.len(),
        sha256: format!("{:x}", sink.hash.finalize()),
    };
    let commit = provision(
        descriptor.descriptor_id.clone(),
        COMMIT_CUSTOM_TYPE,
        &descriptor,
    )?;
    preflight(&commit)?;
    for (index, bytes) in sink.chunks.iter().enumerate() {
        preflight(&provision(
            chunk_id(&descriptor.body_id, index),
            CHUNK_CUSTOM_TYPE,
            &chunk(&descriptor, index, bytes),
        )?)?;
    }
    let mut previous = anchor;
    for (index, bytes) in sink.chunks.into_iter().enumerate() {
        let entry = provision(
            chunk_id(&descriptor.body_id, index),
            CHUNK_CUSTOM_TYPE,
            &chunk(&descriptor, index, &bytes),
        )?;
        append_checked(session, lane, &previous, entry).await?;
        previous = chunk_id(&descriptor.body_id, index);
    }
    append_checked(session, lane, &previous, commit).await?;
    Ok(descriptor)
}
async fn append_checked(
    session: &Session,
    lane: &str,
    previous: &str,
    entry: ProvisionedEntry,
) -> SessionResult<()> {
    let revision = session.storage().history_revision().await?;
    let current = leaf(session, lane).await?;
    if current != previous {
        member(session, &current, previous).await?;
    }
    session
        .append_entry_if_revision(entry, lane, revision)
        .await?;
    Ok(())
}
fn parse<T: DeserializeOwned>(entry: Entry, custom_type: &str) -> SessionResult<T> {
    if encoded_size(&entry)? > MAX_ENTRY_BYTES {
        return Err(invalid("oversized stored body entry"));
    }
    match entry {
        Entry::Custom(c) if c.custom_type == custom_type => {
            serde_json::from_value(c.data.ok_or_else(|| invalid("missing body data"))?)
                .map_err(|e| invalid(&e.to_string()))
        }
        _ => Err(invalid("incorrect body entry type")),
    }
}
pub(crate) fn validate_descriptor(d: &BodyDescriptor, requested: &str) -> SessionResult<()> {
    id(&d.child_id)?;
    if Uuid::parse_str(&d.body_id)
        .ok()
        .filter(|u| u.to_string() == d.body_id && u.get_version_num() == 4)
        .is_none()
        || d.version != 1
        || d.descriptor_id != requested
        || d.descriptor_id != commit_id(&d.body_id)
        || d.codec != "native-json-v1"
        || d.encoding != "base64"
        || d.chunk_bytes != CHUNK_BYTES
        || !(1..=MAX_BODY_BYTES).contains(&d.byte_length)
        || !(1..=MAX_CHUNKS).contains(&d.chunks)
        || d.chunks != d.byte_length.div_ceil(CHUNK_BYTES)
        || !valid_hash(&d.sha256)
    {
        return Err(invalid("invalid body descriptor"));
    }
    Ok(())
}

/// Read only exact-ID descriptor/child/chunks on the pinned owning branch.
/// Does not read snapshots, history, unrelated bodies, or any source-session data.
pub async fn read_body_page(
    session: &Session,
    lane: &str,
    query: &BodyPageQuery,
) -> SessionResult<BodyPage> {
    let session = session.pin(session.id_generator().clone());
    if !(1..=8).contains(&query.limit) || !(1..=MAX_PAGE_BYTES).contains(&query.max_bytes) {
        return Err(SessionError::invalid_query(
            "body page requires count 1..=8 and bytes 1..=65536",
        ));
    }
    id(&query.descriptor_id)?;
    let current = leaf(&session, lane).await?;
    let selected = query
        .leaf_id
        .as_ref()
        .or_else(|| query.cursor.as_ref().map(|c| &c.leaf_id))
        .unwrap_or(&current)
        .clone();
    id(&selected)?;
    if selected != current {
        member(&session, &current, &selected).await?;
    }
    let session_id = session.get_metadata().await?.id;
    id(&session_id)?;
    let start = if let Some(cursor) = &query.cursor {
        if cursor.session_id != session_id
            || cursor.lane != lane
            || cursor.leaf_id != selected
            || cursor.descriptor_id != query.descriptor_id
        {
            return Err(SessionError::invalid_query(
                "foreign or rebased body cursor",
            ));
        }
        cursor.next_index
    } else {
        0
    };
    // Count-one exact branch membership BEFORE resolving any chunk.
    let descriptor: BodyDescriptor = parse(
        member(&session, &selected, &query.descriptor_id).await?,
        COMMIT_CUSTOM_TYPE,
    )?;
    validate_descriptor(&descriptor, &query.descriptor_id)?;
    let child_entry = member(&session, &descriptor.descriptor_id, &descriptor.child_id).await?;
    child(&child_entry)?;
    if start >= descriptor.chunks {
        return Err(SessionError::invalid_query(
            "body cursor index out of range",
        ));
    }
    let mut page = BodyPage {
        total_bytes: descriptor.byte_length,
        descriptor,
        leaf_id: selected.clone(),
        byte_offset: start * CHUNK_BYTES,
        byte_length: 0,
        chunks: vec![],
        next_cursor: None,
    };
    let mut next = start;
    while next < page.descriptor.chunks && page.chunks.len() < query.limit {
        let expected = (page.descriptor.byte_length - next * CHUNK_BYTES).min(CHUNK_BYTES);
        if page.byte_length + expected > query.max_bytes {
            if page.chunks.is_empty() {
                return Err(invalid("body chunk exceeds decoded page budget"));
            }
            break;
        }
        // A committed chunk must precede the descriptor on its ancestor chain,
        // not merely occur later on the selected lane.
        let stored_entry = member(
            &session,
            &page.descriptor.descriptor_id,
            &chunk_id(&page.descriptor.body_id, next),
        )
        .await?;
        if stored_entry.seq() <= child_entry.seq() {
            return Err(invalid("body chunk precedes its child begin entry"));
        }
        let stored: StoredChunk = parse(stored_entry, CHUNK_CUSTOM_TYPE)?;
        if stored.body_id != page.descriptor.body_id
            || stored.child_id != page.descriptor.child_id
            || stored.index != next
            || stored.data.len() != expected.div_ceil(3) * 4
            || !valid_hash(&stored.sha256)
        {
            return Err(invalid("incoherent body chunk"));
        }
        let bytes = STANDARD
            .decode(&stored.data)
            .map_err(|_| invalid("invalid chunk base64"))?;
        if bytes.len() != expected || digest(&bytes) != stored.sha256 {
            return Err(invalid("corrupt body chunk"));
        }
        page.chunks.push(BodyPageChunk {
            index: next,
            byte_offset: next * CHUNK_BYTES,
            byte_length: expected,
            data: stored.data,
            sha256: stored.sha256,
        });
        page.byte_length += expected;
        next += 1;
    }
    if next < page.descriptor.chunks {
        page.next_cursor = Some(BodyCursor {
            session_id,
            lane: lane.into(),
            leaf_id: selected,
            descriptor_id: query.descriptor_id.clone(),
            next_index: next,
        });
    }
    if encoded_size(&page)? > MAX_RESPONSE_BYTES {
        return Err(invalid("body response exceeds 128 KiB"));
    }
    Ok(page)
}
