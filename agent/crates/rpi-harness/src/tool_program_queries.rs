//! Bounded, read-only native child inspection. Cursors bind a branch snapshot,
//! not a credential. Native metadata ancestry may grow with history; these are
//! payload/response bounds, not RSS bounds. No execution, repair or source fallback.
use std::{
    collections::HashSet,
    io::{self, Write},
};

use rpi_agent::AgentMessage;
use rpi_ai::types::Content;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    error::{SessionError, SessionResult},
    session::{types::*, Session},
    tool_program_body::{
        self, BodyDescriptor, BodyKind, BodyPage, BodyPageQuery, COMMIT_CUSTOM_TYPE,
    },
    tool_program_journal::{Begin, Phase, PhaseKind, BEGIN_CUSTOM_TYPE, PHASE_CUSTOM_TYPE},
};

pub const MAX_COUNT: usize = 32;
pub const MAX_PHASE_CANDIDATES: usize = 128;
pub const MAX_RESPONSE_BYTES: usize = 128 * 1024;
pub const MAX_METADATA_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    OldestFirst,
    NewestFirst,
}
impl Direction {
    fn order(self) -> EntryOrder {
        match self {
            Self::OldestFirst => EntryOrder::OldestFirst,
            Self::NewestFirst => EntryOrder::NewestFirst,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildCursor {
    pub session_id: String,
    pub lane: String,
    pub leaf_id: String,
    pub direction: Direction,
    /// None for begin pages, Some for the selected child's phase scan.
    pub child_id: Option<String>,
    pub after_entry_id: String,
    pub after_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildPageQuery {
    pub leaf_id: Option<String>,
    pub cursor: Option<ChildCursor>,
    pub direction: Direction,
    pub limit: usize,
    /// Complete encoded page, including envelope and cursor; no first-item exception.
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildPhaseQuery {
    pub child_id: String,
    pub leaf_id: Option<String>,
    pub cursor: Option<ChildCursor>,
    pub direction: Direction,
    pub limit: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildOrigin {
    pub run_id: String,
    pub assistant_entry_id: String,
    pub tool_index: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChildState {
    Unknown,
}
/// How the original child origin was verified in this destination session.
/// This is provenance over trusted native transcripts and fork metadata, not
/// protection against forgery by a writer with raw storage access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChildOriginVerification {
    NativeCheckpoint,
    /// Historical copied begin; no destination execution/completion is implied.
    ForkCopy,
}
/// Authoritative begin metadata only. State is deliberately not inferred from
/// a bounded window; missing final/close never means successful completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildSummary {
    pub version: u8,
    pub child_id: String,
    pub seq: u64,
    pub origin: ChildOrigin,
    pub origin_verification: ChildOriginVerification,
    pub root_call_id: String,
    pub root_tool_name: String,
    pub tool_name: String,
    pub ordinal: u32,
    pub parent_child_id: Option<String>,
    pub state: ChildState,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChildPhaseKind {
    Requested,
    Prepared,
    Update,
    Final,
    Closed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildPhase {
    pub entry_id: String,
    pub seq: u64,
    pub version: u8,
    pub child_id: String,
    pub index: u64,
    pub phase: ChildPhaseKind,
    pub descriptor: Option<BodyDescriptor>,
    pub is_error: Option<bool>,
    pub cancelled: Option<bool>,
    pub failed: Option<bool>,
    pub interrupted: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildPage {
    pub leaf_id: Option<String>,
    pub children: Vec<ChildSummary>,
    pub next_cursor: Option<ChildCursor>,
    pub scan_exhausted: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildPhasePage {
    pub leaf_id: String,
    pub child_id: String,
    pub phases: Vec<ChildPhase>,
    pub next_cursor: Option<ChildCursor>,
    /// Candidates consumed, including other children. At most 128 per call.
    pub scanned: usize,
    /// Exhaustion of this pinned scan, NOT completion of the child lifecycle.
    pub scan_exhausted: bool,
    pub state: ChildState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildBodyCursor {
    /// Body chunks are always traversed in ascending index order.
    pub direction: Direction,
    pub child_id: String,
    pub phase_entry_id: String,
    pub body: tool_program_body::BodyCursor,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildBodyQuery {
    pub child_id: String,
    pub phase_entry_id: String,
    pub descriptor_id: String,
    pub leaf_id: Option<String>,
    pub cursor: Option<ChildBodyCursor>,
    pub limit: usize,
    /// Decoded body bytes, as in BodyPageQuery (1..=65536).
    pub max_bytes: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildBodyPage {
    pub child_id: String,
    pub phase_entry_id: String,
    pub body: BodyPage,
    pub next_cursor: Option<ChildBodyCursor>,
}

fn invalid(s: &str) -> SessionError {
    SessionError::invalid_query(s)
}
fn id(s: &str) -> SessionResult<()> {
    if s.is_empty() || s.len() > 512 || s.chars().any(char::is_control) {
        return Err(invalid("invalid child query identity"));
    }
    Ok(())
}
fn native_id(s: &str, prefix: &str) -> SessionResult<()> {
    id(s)?;
    let valid = s
        .strip_prefix(prefix)
        .and_then(|s| Uuid::parse_str(s).ok().map(|u| (s, u)))
        .is_some_and(|(s, u)| u.get_version_num() == 4 && u.to_string() == s);
    if !valid {
        return Err(invalid("invalid native journal identity"));
    }
    Ok(())
}
struct Counter(usize);
impl Write for Counter {
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
fn size<T: Serialize + ?Sized>(v: &T) -> SessionResult<usize> {
    let mut c = Counter(0);
    serde_json::to_writer(&mut c, v).map_err(|_| invalid("child encoding failed"))?;
    Ok(c.0)
}
fn budget(limit: usize, bytes: usize) -> SessionResult<()> {
    if !(1..=MAX_COUNT).contains(&limit) || !(1..=MAX_RESPONSE_BYTES).contains(&bytes) {
        return Err(invalid(
            "child query requires count 1..=32 and bytes 1..=131072",
        ));
    }
    Ok(())
}
fn parse<T: DeserializeOwned>(entry: &Entry, typ: &str) -> SessionResult<T> {
    let Entry::Custom(c) = entry else {
        return Err(invalid("not a native journal entry"));
    };
    id(&c.base.id)?;
    if let Some(parent) = &c.base.parent_id {
        id(parent)?;
    }
    if c.base.entry_type != "custom" {
        return Err(invalid("invalid journal entry envelope"));
    }
    if c.custom_type != typ || size(&c.data)? > MAX_METADATA_BYTES {
        return Err(invalid("invalid journal metadata type or size"));
    }
    serde_json::from_value(
        c.data
            .clone()
            .ok_or_else(|| invalid("missing journal metadata"))?,
    )
    .map_err(|_| invalid("invalid journal schema"))
}
async fn exact(s: &Session, leaf: &str, entry_id: &str, typ: Option<&str>) -> SessionResult<Entry> {
    id(entry_id)?;
    s.storage()
        .find_entries_on_branch(
            &EntryQuery {
                entry_id: Some(entry_id.into()),
                // The only non-custom lookup is the assistant. A forged origin
                // must never hydrate an archival chunk as a supposed assistant.
                entry_type: Some(if typ.is_some() { "custom" } else { "message" }),
                custom_type: typ.map(str::to_owned),
                limit: Some(1),
                ..Default::default()
            },
            &BranchBounds::default(),
            leaf,
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| invalid("entry not on selected branch or incorrect type"))
}
/// Native stores implement metadata-only ancestry. Do not replace with history,
/// messages, or an unfiltered entry query. Compatibility storage implementations
/// must themselves provide metadata-only branch_ancestry for this guarantee.
async fn ancestor(s: &Session, leaf: &str, target: &str) -> SessionResult<()> {
    id(target)?;
    if !s
        .storage()
        .branch_ancestry(
            &BranchBounds {
                stop_at_id: Some(target.into()),
                ..Default::default()
            },
            leaf,
        )
        .await?
        .iter()
        .any(|e| e.id == target)
    {
        return Err(invalid("selected leaf is not on current branch"));
    }
    Ok(())
}
struct Binding {
    session_id: String,
    lane: String,
    leaf: Option<String>,
}
async fn binding(s: &Session, lane: &str, leaf: Option<&str>) -> SessionResult<Binding> {
    id(lane)?;
    let current = s
        .get_lanes()
        .await?
        .into_iter()
        .find(|p| p.lane == lane)
        .ok_or_else(|| invalid("unknown child lane"))?
        .leaf_id;
    let selected = leaf.map(str::to_owned).or_else(|| current.clone());
    if let Some(selected) = &selected {
        id(selected)?;
        let current = current
            .as_deref()
            .ok_or_else(|| invalid("selected leaf on empty lane"))?;
        if selected != current {
            ancestor(s, current, selected).await?;
        }
    }
    let session_id = s.get_metadata().await?.id;
    id(&session_id)?;
    Ok(Binding {
        session_id,
        lane: lane.into(),
        leaf: selected,
    })
}
impl Binding {
    fn cursor(&self, direction: Direction, child: Option<&str>, entry: &Entry) -> ChildCursor {
        ChildCursor {
            session_id: self.session_id.clone(),
            lane: self.lane.clone(),
            leaf_id: self.leaf.clone().expect("nonempty scan"),
            direction,
            child_id: child.map(str::to_owned),
            after_entry_id: entry.id().into(),
            after_seq: entry.seq(),
        }
    }
}
async fn cursor(
    s: &Session,
    b: &Binding,
    c: Option<&ChildCursor>,
    dir: Direction,
    child: Option<&str>,
    typ: &str,
) -> SessionResult<()> {
    if let Some(c) = c {
        id(&c.session_id)?;
        id(&c.lane)?;
        id(&c.leaf_id)?;
        id(&c.after_entry_id)?;
        if c.session_id != b.session_id
            || c.lane != b.lane
            || Some(&c.leaf_id) != b.leaf.as_ref()
            || c.direction != dir
            || c.child_id.as_deref() != child
        {
            return Err(invalid("foreign or rebased child cursor"));
        }
        let entry = exact(s, &c.leaf_id, &c.after_entry_id, Some(typ)).await?;
        if c.after_seq != entry.seq() {
            return Err(invalid("child cursor sequence mismatch"));
        }
        if let Some(child) = child {
            let begin = exact(s, &c.leaf_id, child, Some(BEGIN_CUSTOM_TYPE)).await?;
            if entry.seq() <= begin.seq() {
                return Err(invalid("phase cursor precedes begin"));
            }
            native_id(entry.id(), "tpphase-")?;
            let phase: Phase = parse(&entry, PHASE_CUSTOM_TYPE)?;
            phase_shape(&phase)?;
        } else {
            begin_authority(s, &c.leaf_id, &entry).await?;
        }
    }
    Ok(())
}
async fn begin_authority(
    s: &Session,
    leaf: &str,
    entry: &Entry,
) -> SessionResult<(Begin, ChildOriginVerification)> {
    let first: Begin = parse(entry, BEGIN_CUSTOM_TYPE)?;
    let mut current = first.clone();
    let mut current_seq = entry.seq();
    let mut seen = HashSet::new();
    for depth in 0..128 {
        native_id(&current.child_id, "tpchild-")?;
        if !seen.insert(current.child_id.clone())
            || current.version != 1
            || (depth == 0 && current.child_id != entry.id())
        {
            return Err(invalid("invalid child begin identity/version/cycle"));
        }
        for v in [
            &current.origin.run_id,
            &current.origin.assistant_entry_id,
            &current.root_call_id,
            &current.root_tool_name,
            &current.tool_name,
        ] {
            id(v)?;
        }
        if current.origin != first.origin
            || current.root_call_id != first.root_call_id
            || current.root_tool_name != first.root_tool_name
        {
            return Err(invalid("foreign child parent origin"));
        }
        match &current.parent_child_id {
            None => break,
            Some(parent) => {
                if depth == 127 {
                    return Err(invalid("child parent traversal exceeds 128"));
                }
                native_id(parent, "tpchild-")?;
                let parent_entry = exact(s, leaf, parent, Some(BEGIN_CUSTOM_TYPE)).await?;
                let parsed: Begin = parse(&parent_entry, BEGIN_CUSTOM_TYPE)?;
                if parsed.child_id != *parent || parent_entry.seq() >= current_seq {
                    return Err(invalid("child parent does not precede child"));
                }
                current_seq = parent_entry.seq();
                current = parsed;
            }
        }
    }
    // The assistant is an actual branch-local message, with tool-call-only ordinal.
    let assistant = exact(s, entry.id(), &first.origin.assistant_entry_id, None).await?;
    let Entry::Message(message) = &assistant else {
        return Err(invalid("child origin is not assistant"));
    };
    let AgentMessage::Assistant(message) = &message.message else {
        return Err(invalid("child origin is not assistant"));
    };
    let call = message
        .content
        .iter()
        .filter_map(|v| match v {
            Content::ToolCall(c) => Some(c),
            _ => None,
        })
        .nth(first.origin.tool_index as usize);
    if assistant.seq() >= current_seq
        || !call.is_some_and(|c| c.id == first.root_call_id && c.name == first.root_tool_name)
    {
        return Err(invalid("child assistant slot mismatch"));
    }
    let roots = s
        .storage()
        .find_records(&RecordQuery {
            record_type: Some("operation_started"),
            run_id: Some(first.origin.run_id.clone()),
            limit: Some(2),
            ..Default::default()
        })
        .await?;
    let starts = s
        .storage()
        .find_records(&RecordQuery {
            record_type: Some("tool_started"),
            run_id: Some(first.origin.run_id.clone()),
            tool_call_id: Some(first.root_call_id.clone()),
            limit: Some(2),
            ..Default::default()
        })
        .await?;
    // Native forks copy entries but not lane records. Only complete absence of
    // BOTH checkpoints permits historical-copy provenance. Partial, ambiguous,
    // or mismatching destination evidence must never downgrade to ForkCopy.
    if roots.is_empty() && starts.is_empty() {
        let metadata = s.get_metadata().await?;
        if let Some(parent) = metadata.parent_session_id.as_deref() {
            id(parent)?;
            if parent != metadata.id {
                return Ok((first, ChildOriginVerification::ForkCopy));
            }
        }
        return Err(invalid("child native root checkpoint unavailable"));
    }
    let [LaneRecord::OperationStarted(root)] = roots.as_slice() else {
        return Err(invalid("child native root run unavailable or ambiguous"));
    };
    let [LaneRecord::ToolStarted(start)] = starts.as_slice() else {
        return Err(invalid("child Never checkpoint unavailable or ambiguous"));
    };
    if !matches!(root.intent, OperationIntent::Run { .. })
        || start.replay != ToolReplay::Never
        || start.assistant_entry_id != first.origin.assistant_entry_id
        || start.tool_index != first.origin.tool_index
        || start.tool_name != first.root_tool_name
        || start.base.lane != root.base.lane
        || root.base.seq >= assistant.seq()
        || assistant.seq() >= start.base.seq
        || start.base.seq >= current_seq
    {
        return Err(invalid("child native root checkpoint mismatch"));
    }
    Ok((first, ChildOriginVerification::NativeCheckpoint))
}
fn phase_shape(p: &Phase) -> SessionResult<()> {
    native_id(&p.child_id, "tpchild-")?;
    if p.version != 1 || ((p.phase == PhaseKind::Requested) != (p.index == 0)) {
        return Err(invalid("invalid phase version/index"));
    }
    let closed = p.phase == PhaseKind::Closed;
    let final_phase = p.phase == PhaseKind::Final;
    if p.failed.is_some() != closed
        || p.interrupted.is_some() != closed
        || p.is_error.is_some() != final_phase
        || p.cancelled.is_some() != final_phase
        || p.descriptor.is_none() != closed
    {
        return Err(invalid("invalid phase flags or descriptor shape"));
    }
    if let Some(d) = &p.descriptor {
        tool_program_body::validate_descriptor(d, &d.descriptor_id)?;
        let kind = match p.phase {
            PhaseKind::Requested => BodyKind::Requested,
            PhaseKind::Prepared => BodyKind::Prepared,
            PhaseKind::Update => BodyKind::Update,
            PhaseKind::Final => BodyKind::Final,
            PhaseKind::Closed => unreachable!(),
        };
        if d.child_id != p.child_id || d.kind != kind {
            return Err(invalid("phase descriptor child/kind mismatch"));
        }
    }
    Ok(())
}
async fn phase_authority(s: &Session, entry: &Entry, begin_entry: &Entry) -> SessionResult<Phase> {
    native_id(entry.id(), "tpphase-")?;
    let p: Phase = parse(entry, PHASE_CUSTOM_TYPE)?;
    phase_shape(&p)?;
    if p.child_id != begin_entry.id() || entry.seq() <= begin_entry.seq() {
        return Err(invalid("phase does not follow selected begin"));
    }
    ancestor(s, entry.id(), begin_entry.id()).await?;
    if let Some(d) = &p.descriptor {
        let commit = exact(s, entry.id(), &d.descriptor_id, Some(COMMIT_CUSTOM_TYPE)).await?;
        let stored: BodyDescriptor = parse(&commit, COMMIT_CUSTOM_TYPE)?;
        if stored != *d || commit.seq() <= begin_entry.seq() || commit.seq() >= entry.seq() {
            return Err(invalid("phase descriptor commit is not authoritative"));
        }
        ancestor(s, &d.descriptor_id, begin_entry.id()).await?;
    }
    Ok(p)
}
fn summary(e: &Entry, (b, verification): (Begin, ChildOriginVerification)) -> ChildSummary {
    ChildSummary {
        version: b.version,
        child_id: b.child_id,
        seq: e.seq(),
        origin: ChildOrigin {
            run_id: b.origin.run_id,
            assistant_entry_id: b.origin.assistant_entry_id,
            tool_index: b.origin.tool_index,
        },
        origin_verification: verification,
        root_call_id: b.root_call_id,
        root_tool_name: b.root_tool_name,
        tool_name: b.tool_name,
        ordinal: b.ordinal,
        parent_child_id: b.parent_child_id,
        state: ChildState::Unknown,
    }
}
fn phase(e: &Entry, p: Phase) -> ChildPhase {
    ChildPhase {
        entry_id: e.id().into(),
        seq: e.seq(),
        version: p.version,
        child_id: p.child_id,
        index: p.index,
        phase: match p.phase {
            PhaseKind::Requested => ChildPhaseKind::Requested,
            PhaseKind::Prepared => ChildPhaseKind::Prepared,
            PhaseKind::Update => ChildPhaseKind::Update,
            PhaseKind::Final => ChildPhaseKind::Final,
            PhaseKind::Closed => ChildPhaseKind::Closed,
        },
        descriptor: p.descriptor,
        is_error: p.is_error,
        cancelled: p.cancelled,
        failed: p.failed,
        interrupted: p.interrupted,
    }
}
async fn candidates(
    s: &Session,
    leaf: &str,
    typ: &str,
    dir: Direction,
    cursor: Option<&ChildCursor>,
    count: usize,
    begin: Option<&Entry>,
) -> SessionResult<Vec<Entry>> {
    let after = cursor
        .map(|c| EntryCursor {
            after_seq: c.after_seq,
        })
        .or_else(|| {
            if dir == Direction::OldestFirst {
                begin.map(|e| EntryCursor { after_seq: e.seq() })
            } else {
                None
            }
        });
    // Oldest-first inclusive bounds have different native semantics: use its
    // exclusive sequence cursor instead. Newest-first can stop at the begin.
    let bounds = BranchBounds {
        stop_at_id: if dir == Direction::NewestFirst {
            begin.map(|e| e.id().into())
        } else {
            None
        },
        ..Default::default()
    };
    s.storage()
        .find_entries_on_branch(
            &EntryQuery {
                custom_type: Some(typ.into()),
                order: Some(dir.order()),
                cursor: after,
                limit: Some(count),
                ..Default::default()
            },
            &bounds,
            leaf,
        )
        .await
}

pub async fn read_child_page(
    session: &Session,
    lane: &str,
    query: &ChildPageQuery,
) -> SessionResult<ChildPage> {
    let s = session.pin(session.id_generator().clone());
    budget(query.limit, query.max_bytes)?;
    let b = binding(
        &s,
        lane,
        query
            .leaf_id
            .as_deref()
            .or_else(|| query.cursor.as_ref().map(|c| c.leaf_id.as_str())),
    )
    .await?;
    cursor(
        &s,
        &b,
        query.cursor.as_ref(),
        query.direction,
        None,
        BEGIN_CUSTOM_TYPE,
    )
    .await?;
    let mut page = ChildPage {
        leaf_id: b.leaf.clone(),
        children: vec![],
        next_cursor: None,
        scan_exhausted: true,
    };
    if let Some(leaf) = &b.leaf {
        let entries = candidates(
            &s,
            leaf,
            BEGIN_CUSTOM_TYPE,
            query.direction,
            query.cursor.as_ref(),
            query.limit,
            None,
        )
        .await?;
        let n = entries.len();
        for (i, e) in entries.iter().enumerate() {
            let begin = begin_authority(&s, leaf, e).await?;
            let previous = page.clone();
            page.children.push(summary(e, begin));
            page.scan_exhausted = n < query.limit && i + 1 == n;
            page.next_cursor = if page.scan_exhausted {
                None
            } else {
                Some(b.cursor(query.direction, None, e))
            };
            if size(&page)? > query.max_bytes {
                page = previous;
                if page.children.is_empty() {
                    return Err(invalid("child item exceeds encoded page budget"));
                }
                page.scan_exhausted = false;
                // Previous cursor identifies exactly the last returned item.
                break;
            }
        }
    }
    if size(&page)? > query.max_bytes {
        return Err(invalid("child envelope exceeds encoded page budget"));
    }
    Ok(page)
}

pub async fn read_child_phase_page(
    session: &Session,
    lane: &str,
    query: &ChildPhaseQuery,
) -> SessionResult<ChildPhasePage> {
    let s = session.pin(session.id_generator().clone());
    budget(query.limit, query.max_bytes)?;
    native_id(&query.child_id, "tpchild-")?;
    let b = binding(
        &s,
        lane,
        query
            .leaf_id
            .as_deref()
            .or_else(|| query.cursor.as_ref().map(|c| c.leaf_id.as_str())),
    )
    .await?;
    cursor(
        &s,
        &b,
        query.cursor.as_ref(),
        query.direction,
        Some(&query.child_id),
        PHASE_CUSTOM_TYPE,
    )
    .await?;
    let leaf = b
        .leaf
        .as_deref()
        .ok_or_else(|| invalid("child unavailable on empty lane"))?;
    let begin_entry = exact(&s, leaf, &query.child_id, Some(BEGIN_CUSTOM_TYPE)).await?;
    begin_authority(&s, leaf, &begin_entry).await?;
    let mut scan_cursor = query.cursor.clone();
    // Ascending scans start at begin; no earlier phase can belong to this child.
    let mut entries = candidates(
        &s,
        leaf,
        PHASE_CUSTOM_TYPE,
        query.direction,
        scan_cursor.as_ref(),
        MAX_PHASE_CANDIDATES,
        Some(&begin_entry),
    )
    .await?;
    let n = entries.len();
    let mut page = ChildPhasePage {
        leaf_id: leaf.into(),
        child_id: query.child_id.clone(),
        phases: vec![],
        next_cursor: None,
        scanned: 0,
        scan_exhausted: n == 0,
        state: ChildState::Unknown,
    };
    let mut last_index = None;
    for (i, e) in entries.drain(..).enumerate() {
        let p: Phase = parse(&e, PHASE_CUSTOM_TYPE)?;
        phase_shape(&p)?;
        let previous = page.clone();
        if p.child_id == query.child_id {
            let p = phase_authority(&s, &e, &begin_entry).await?;
            if last_index.is_some_and(|last| match query.direction {
                Direction::OldestFirst => p.index <= last,
                Direction::NewestFirst => p.index >= last,
            }) {
                return Err(invalid("nonmonotonic child phase index"));
            }
            last_index = Some(p.index);
            page.phases.push(phase(&e, p));
        }
        page.scanned += 1;
        page.scan_exhausted = i + 1 == n && n < MAX_PHASE_CANDIDATES;
        scan_cursor = Some(b.cursor(query.direction, Some(&query.child_id), &e));
        page.next_cursor = if page.scan_exhausted {
            None
        } else {
            scan_cursor.clone()
        };
        if size(&page)? > query.max_bytes {
            page = previous;
            if page.phases.is_empty() {
                return Err(invalid("child phase/envelope exceeds encoded page budget"));
            }
            page.scan_exhausted = false;
            break;
        }
        if page.phases.len() == query.limit {
            break;
        }
    }
    if size(&page)? > query.max_bytes {
        return Err(invalid("child phase envelope exceeds encoded page budget"));
    }
    Ok(page)
}

pub async fn read_child_body_page(
    session: &Session,
    lane: &str,
    query: &ChildBodyQuery,
) -> SessionResult<ChildBodyPage> {
    let s = session.pin(session.id_generator().clone());
    if !(1..=8).contains(&query.limit)
        || !(1..=tool_program_body::MAX_PAGE_BYTES).contains(&query.max_bytes)
    {
        return Err(invalid(
            "child body requires count 1..=8 and decoded bytes 1..=65536",
        ));
    }
    native_id(&query.child_id, "tpchild-")?;
    native_id(&query.phase_entry_id, "tpphase-")?;
    id(&query.descriptor_id)?;
    if let Some(c) = &query.cursor {
        for value in [
            &c.child_id,
            &c.phase_entry_id,
            &c.body.session_id,
            &c.body.lane,
            &c.body.leaf_id,
            &c.body.descriptor_id,
        ] {
            id(value)?;
        }
        if c.direction != Direction::OldestFirst
            || c.child_id != query.child_id
            || c.phase_entry_id != query.phase_entry_id
        {
            return Err(invalid("foreign child body cursor"));
        }
    }
    let b = binding(
        &s,
        lane,
        query
            .leaf_id
            .as_deref()
            .or_else(|| query.cursor.as_ref().map(|c| c.body.leaf_id.as_str())),
    )
    .await?;
    let leaf = b
        .leaf
        .as_deref()
        .ok_or_else(|| invalid("child unavailable on empty lane"))?;
    let begin_entry = exact(&s, leaf, &query.child_id, Some(BEGIN_CUSTOM_TYPE)).await?;
    begin_authority(&s, leaf, &begin_entry).await?;
    let phase_entry = exact(&s, leaf, &query.phase_entry_id, Some(PHASE_CUSTOM_TYPE)).await?;
    let phase = phase_authority(&s, &phase_entry, &begin_entry).await?;
    let descriptor = phase
        .descriptor
        .ok_or_else(|| invalid("closed phase has no body"))?;
    if descriptor.descriptor_id != query.descriptor_id {
        return Err(invalid("body descriptor not authorized by child phase"));
    }
    let mut body = tool_program_body::read_body_page(
        &s,
        lane,
        &BodyPageQuery {
            descriptor_id: query.descriptor_id.clone(),
            leaf_id: Some(leaf.into()),
            cursor: query.cursor.as_ref().map(|c| c.body.clone()),
            limit: query.limit,
            max_bytes: query.max_bytes,
        },
    )
    .await?;
    let next_cursor = body.next_cursor.take().map(|body| ChildBodyCursor {
        direction: Direction::OldestFirst,
        child_id: query.child_id.clone(),
        phase_entry_id: query.phase_entry_id.clone(),
        body,
    });
    let page = ChildBodyPage {
        child_id: query.child_id.clone(),
        phase_entry_id: query.phase_entry_id.clone(),
        body,
        next_cursor,
    };
    if size(&page)? > MAX_RESPONSE_BYTES {
        return Err(invalid("child body response exceeds 128 KiB"));
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        session::{
            jsonl::{JsonlSessionStorage, JsonlV4Header},
            DefaultIdGenerator, InMemorySessionStorage, SystemClock,
        },
        tool_program_journal::{ChildOrigin as StoredOrigin, Correlate, JournalObserver},
    };
    use rpi_agent::{
        tool_program_observation::{ToolProgramChild, ToolProgramObservation, ToolProgramObserver},
        AgentToolResult,
    };
    use rpi_ai::{
        providers::faux::{faux_assistant_message, faux_tool_call},
        types::StopReason,
    };
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn memory() -> Session {
        Session::new(
            Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: "queries".into(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )),
            None,
        )
    }
    fn base(id: &str) -> RecordBase {
        RecordBase {
            id: id.into(),
            seq: 0,
            lane: "main".into(),
            timestamp: 0,
        }
    }
    async fn setup(s: &Session) -> JournalObserver {
        s.append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("run"),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
        let mut m = faux_assistant_message("interleaved", StopReason::ToolUse);
        let mut call = faux_tool_call("program", json!({}));
        if let Content::ToolCall(c) = &mut call {
            c.id = "root-call".into();
        }
        m.content.push(call);
        s.append_entry(
            ProvisionedEntry {
                id: "assistant".into(),
                kind: ProvisionedKind::Message {
                    message: AgentMessage::Assistant(Box::new(m)),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
        s.append_record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: base("started"),
            run_id: "run".into(),
            assistant_entry_id: "assistant".into(),
            tool_index: 0,
            tool_call_id: "root-call".into(),
            tool_name: "program".into(),
            effective_args: json!({}),
            result_entry_id: "result".into(),
            replay: ToolReplay::Never,
        }))
        .await
        .unwrap();
        let correlate: Correlate = Arc::new(|_| {
            Box::pin(async {
                Ok(StoredOrigin {
                    run_id: "run".into(),
                    assistant_entry_id: "assistant".into(),
                    tool_index: 0,
                })
            })
        });
        JournalObserver::new(s.clone(), "main".into(), correlate, Arc::new(|_| {}))
    }
    async fn begin(
        j: &JournalObserver,
        request: &str,
        parent: &str,
        args: Value,
    ) -> Arc<dyn ToolProgramObservation> {
        j.begin(
            ToolProgramChild {
                root_id: "root-call".into(),
                root_name: "program".into(),
                parent_id: parent.into(),
                request_id: request.into(),
                name: "read".into(),
                ordinal: 1,
            },
            &args,
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }
    fn q() -> ChildPageQuery {
        ChildPageQuery {
            leaf_id: None,
            cursor: None,
            direction: Direction::OldestFirst,
            limit: 32,
            max_bytes: MAX_RESPONSE_BYTES,
        }
    }
    fn pq(child: &str) -> ChildPhaseQuery {
        ChildPhaseQuery {
            child_id: child.into(),
            leaf_id: None,
            cursor: None,
            direction: Direction::OldestFirst,
            limit: 32,
            max_bytes: MAX_RESPONSE_BYTES,
        }
    }
    fn bq(child: &str, p: &ChildPhase) -> ChildBodyQuery {
        ChildBodyQuery {
            child_id: child.into(),
            phase_entry_id: p.entry_id.clone(),
            descriptor_id: p.descriptor.as_ref().unwrap().descriptor_id.clone(),
            leaf_id: None,
            cursor: None,
            limit: 1,
            max_bytes: 65536,
        }
    }
    async fn raw(s: &Session, id: String, typ: &str, data: Value) {
        s.storage()
            .append_entry(
                ProvisionedEntry {
                    id,
                    kind: ProvisionedKind::Custom {
                        custom_type: typ.into(),
                        data: Some(data),
                    },
                },
                "main",
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn genuine_journal_paging_bytes_unknown_and_snapshot() {
        let s = memory();
        let j = setup(&s).await;
        let a = begin(&j, "a", "root-call", json!({"huge":"x".repeat(70000)})).await;
        let snapshot = s.view("main").get_leaf_id().await.unwrap().unwrap();
        let b = begin(&j, "b", "a", json!({})).await;
        b.finish(&AgentToolResult::text("nested"), false, false)
            .await
            .unwrap();
        b.close(None).await.unwrap();
        // Deliberately leave a without final/close. Reading must not infer success.
        let p = read_child_page(&s, "main", &q()).await.unwrap();
        assert_eq!(p.children.len(), 2);
        assert!(p.children.iter().all(|c| c.state == ChildState::Unknown
            && c.origin_verification == ChildOriginVerification::NativeCheckpoint));
        assert_eq!(
            serde_json::to_value(&p.children[0]).unwrap()["originVerification"],
            "nativeCheckpoint"
        );
        assert_eq!(p.children[1].parent_child_id.as_deref(), Some(a.id()));
        let mut small = q();
        small.limit = 1;
        let first = read_child_page(&s, "main", &small).await.unwrap();
        small.cursor = first.next_cursor.clone();
        let second = read_child_page(&s, "main", &small).await.unwrap();
        assert_eq!(second.children[0].child_id, b.id());
        let mut bytes = q();
        bytes.max_bytes = size(&first).unwrap();
        let page = read_child_page(&s, "main", &bytes).await.unwrap();
        assert_eq!(page.children.len(), 1);
        assert!(size(&page).unwrap() <= bytes.max_bytes);
        bytes.cursor = page.next_cursor;
        bytes.max_bytes = MAX_RESPONSE_BYTES;
        assert_eq!(
            read_child_page(&s, "main", &bytes).await.unwrap().children[0].child_id,
            b.id()
        );
        let mut tiny = q();
        tiny.max_bytes = 100;
        assert!(read_child_page(&s, "main", &tiny).await.is_err());
        let phases = read_child_phase_page(&s, "main", &pq(a.id()))
            .await
            .unwrap();
        assert_eq!(phases.phases.len(), 1);
        assert_eq!(phases.state, ChildState::Unknown);
        let mut body = bq(a.id(), &phases.phases[0]);
        let bp = read_child_body_page(&s, "main", &body).await.unwrap();
        assert!(bp.next_cursor.is_some());
        body.cursor = bp.next_cursor;
        assert!(read_child_body_page(&s, "main", &body).await.is_ok());
        let mut at = q();
        at.leaf_id = Some(snapshot.clone());
        assert_eq!(
            read_child_page(&s, "main", &at)
                .await
                .unwrap()
                .children
                .len(),
            1
        );
        let mut pp = pq(b.id());
        pp.leaf_id = Some(snapshot);
        assert!(read_child_phase_page(&s, "main", &pp).await.is_err());
        drop(a);
    }

    #[tokio::test]
    async fn phase_bytes_resume_before_unreturned_match_and_pin_leaf() {
        let s = memory();
        let j = setup(&s).await;
        let a = begin(&j, "a", "root-call", json!({})).await;
        let snapshot = s.view("main").get_leaf_id().await.unwrap().unwrap();
        a.finish(&AgentToolResult::text("final"), true, true)
            .await
            .unwrap();
        a.close(None).await.unwrap();
        let mut query = pq(a.id());
        query.limit = 1;
        let first = read_child_phase_page(&s, "main", &query).await.unwrap();
        query.limit = 32;
        query.max_bytes = size(&first).unwrap();
        let bounded = read_child_phase_page(&s, "main", &query).await.unwrap();
        assert_eq!(bounded.phases.len(), 1);
        assert!(bounded.next_cursor.is_some());
        assert!(size(&bounded).unwrap() <= query.max_bytes);
        query.cursor = bounded.next_cursor;
        query.max_bytes = MAX_RESPONSE_BYTES;
        let rest = read_child_phase_page(&s, "main", &query).await.unwrap();
        assert_eq!(rest.phases.len(), 2);
        assert_eq!(rest.phases[0].phase, ChildPhaseKind::Final);
        assert_eq!(rest.phases[0].is_error, Some(true));
        assert_eq!(rest.phases[0].cancelled, Some(true));
        query.cursor = None;
        query.leaf_id = Some(snapshot.clone());
        let old = read_child_phase_page(&s, "main", &query).await.unwrap();
        assert_eq!(old.phases.len(), 1);
        let mut body = bq(a.id(), &rest.phases[0]);
        body.leaf_id = Some(snapshot);
        assert!(read_child_body_page(&s, "main", &body).await.is_err());
        query.max_bytes = 100;
        assert!(read_child_phase_page(&s, "main", &query).await.is_err());
    }

    #[tokio::test]
    async fn sparse_128_candidate_continuations_and_reverse_order() {
        let s = memory();
        let j = setup(&s).await;
        let a = begin(&j, "a", "root-call", json!({})).await;
        for i in 0..44 {
            let o = begin(&j, &format!("other{i}"), "root-call", json!({})).await;
            o.finish(&AgentToolResult::text("ok"), false, false)
                .await
                .unwrap();
            o.close(None).await.unwrap();
        }
        a.finish(&AgentToolResult::text("done"), false, false)
            .await
            .unwrap();
        a.close(None).await.unwrap();
        let mut query = pq(a.id());
        query.limit = 1;
        let first = read_child_phase_page(&s, "main", &query).await.unwrap();
        assert_eq!(first.phases[0].index, 0);
        query.cursor = first.next_cursor;
        let sparse = read_child_phase_page(&s, "main", &query).await.unwrap();
        assert_eq!(sparse.scanned, 128);
        assert!(sparse.phases.is_empty());
        assert!(!sparse.scan_exhausted);
        assert!(sparse.next_cursor.is_some());
        query.cursor = sparse.next_cursor;
        let next = read_child_phase_page(&s, "main", &query).await.unwrap();
        assert_eq!(next.phases[0].phase, ChildPhaseKind::Final);
        query.cursor = next.next_cursor;
        assert_eq!(
            read_child_phase_page(&s, "main", &query)
                .await
                .unwrap()
                .phases[0]
                .phase,
            ChildPhaseKind::Closed
        );
        let mut reverse = pq(a.id());
        reverse.direction = Direction::NewestFirst;
        reverse.limit = 2;
        let page = read_child_phase_page(&s, "main", &reverse).await.unwrap();
        assert_eq!(
            page.phases.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![2, 1]
        );
        reverse.cursor = page.next_cursor;
        let sparse = read_child_phase_page(&s, "main", &reverse).await.unwrap();
        assert!(sparse.phases.is_empty());
        assert!(sparse.next_cursor.is_some());
        reverse.cursor = sparse.next_cursor;
        assert_eq!(
            read_child_phase_page(&s, "main", &reverse)
                .await
                .unwrap()
                .phases[0]
                .index,
            0
        );
        let page = read_child_page(&s, "main", &q()).await.unwrap();
        assert_eq!(page.children.len(), 32);
        assert!(page.next_cursor.is_some());
    }

    #[tokio::test]
    async fn foreign_cursors_rebased_branches_and_body_anchor_fail_closed() {
        let s = memory();
        let j = setup(&s).await;
        let a = begin(&j, "a", "root-call", json!("x".repeat(70000))).await;
        let phases = read_child_phase_page(&s, "main", &pq(a.id()))
            .await
            .unwrap();
        let phase = &phases.phases[0];
        let mut body = bq(a.id(), phase);
        let first = read_child_body_page(&s, "main", &body).await.unwrap();
        body.cursor = first.next_cursor;
        body.cursor.as_mut().unwrap().body.session_id = "foreign".into();
        assert!(read_child_body_page(&s, "main", &body).await.is_err());
        let mut one = q();
        one.limit = 1;
        let p = read_child_page(&s, "main", &one).await.unwrap();
        for field in 0..5 {
            let mut c = p.next_cursor.clone().unwrap();
            match field {
                0 => c.session_id = "foreign".into(),
                1 => c.lane = "foreign".into(),
                2 => c.direction = Direction::NewestFirst,
                3 => c.child_id = Some(a.id().into()),
                _ => c.after_seq += 1,
            }
            one.cursor = Some(c);
            assert!(read_child_page(&s, "main", &one).await.is_err());
        }
        let orphan = tool_program_body::append_body(
            &s,
            "main",
            a.id(),
            BodyKind::Requested,
            &json!("orphan"),
            1024,
        )
        .await
        .unwrap();
        let mut wrong = bq(a.id(), phase);
        wrong.descriptor_id = orphan.descriptor_id;
        assert!(read_child_body_page(&s, "main", &wrong).await.is_err());
        let forkpoint = phase.entry_id.clone();
        let d = tool_program_body::append_body(
            &s,
            "main",
            a.id(),
            BodyKind::Final,
            &json!("sibling"),
            1024,
        )
        .await
        .unwrap();
        s.storage()
            .move_lane("main", Some(&forkpoint))
            .await
            .unwrap();
        let forged = format!("tpphase-{}", Uuid::new_v4());
        raw(&s, forged.clone(), PHASE_CUSTOM_TYPE, json!({"version":1,"childId":a.id(),"index":1,"phase":"final","descriptor":d,"isError":false,"cancelled":false})).await;
        let wrong = ChildBodyQuery {
            phase_entry_id: forged,
            descriptor_id: d.descriptor_id,
            ..bq(a.id(), phase)
        };
        assert!(read_child_body_page(&s, "main", &wrong).await.is_err());
        s.storage()
            .move_lane("main", Some("assistant"))
            .await
            .unwrap();
        one.cursor = p.next_cursor;
        assert!(read_child_page(&s, "main", &one).await.is_err());
        drop(a);
    }

    #[tokio::test]
    async fn malformed_begin_phase_and_strict_queries_rejected() {
        assert!(serde_json::from_value::<ChildPageQuery>(json!({"leafId":null,"cursor":null,"direction":"oldestFirst","limit":1,"maxBytes":1000,"extra":true})).is_err());
        for mutation in 0..6 {
            let s = memory();
            let j = setup(&s).await;
            let a = begin(&j, "a", "root-call", json!({})).await;
            let original = exact(&s, a.id(), a.id(), Some(BEGIN_CUSTOM_TYPE))
                .await
                .unwrap();
            let b: Begin = parse(&original, BEGIN_CUSTOM_TYPE).unwrap();
            let id = format!("tpchild-{}", Uuid::new_v4());
            let mut value = serde_json::to_value(b).unwrap();
            value["childId"] = json!(id);
            match mutation {
                0 => value["origin"]["toolIndex"] = json!(1),
                1 => value["rootToolName"] = json!("wrong"),
                2 => value["version"] = json!(2),
                3 => value["parentChildId"] = json!(id),
                4 => value["unexpected"] = json!(true),
                _ => value["toolName"] = json!("x".repeat(MAX_METADATA_BYTES)),
            }
            raw(&s, id, BEGIN_CUSTOM_TYPE, value).await;
            assert!(
                read_child_page(&s, "main", &q()).await.is_err(),
                "mutation {mutation}"
            );
            drop(a);
        }
        let s = memory();
        let j = setup(&s).await;
        let a = begin(&j, "a", "root-call", json!({})).await;
        raw(
            &s,
            format!("tpphase-{}", Uuid::new_v4()),
            PHASE_CUSTOM_TYPE,
            json!({"version":1,"childId":a.id(),"index":1,"phase":"closed","failed":false}),
        )
        .await;
        assert!(read_child_phase_page(&s, "main", &pq(a.id()))
            .await
            .is_err());
        drop(a);
        let empty = memory();
        assert!(read_child_page(&empty, "main", &q())
            .await
            .unwrap()
            .children
            .is_empty());
    }

    async fn assert_copy(s: &Session, child: &str, expected: &[u8]) {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let page = read_child_page(s, "main", &q()).await.unwrap();
        let summary = page.children.iter().find(|c| c.child_id == child).unwrap();
        assert_eq!(
            summary.origin_verification,
            ChildOriginVerification::ForkCopy
        );
        assert_eq!(summary.state, ChildState::Unknown);
        assert_eq!(summary.origin.run_id, "run");
        assert_eq!(
            serde_json::to_value(summary).unwrap()["originVerification"],
            "forkCopy"
        );
        let phases = read_child_phase_page(s, "main", &pq(child)).await.unwrap();
        assert_eq!(phases.state, ChildState::Unknown);
        assert_eq!(phases.phases.len(), 3);
        let mut query = bq(child, &phases.phases[0]);
        let mut page = read_child_body_page(s, "main", &query).await.unwrap();
        assert!(page.next_cursor.is_some());
        let mut bytes = Vec::new();
        loop {
            for chunk in page.body.chunks {
                bytes.extend(STANDARD.decode(chunk.data).unwrap());
            }
            query.cursor = page.next_cursor;
            if query.cursor.is_none() {
                break;
            }
            page = read_child_body_page(s, "main", &query).await.unwrap();
        }
        assert_eq!(bytes, expected);
        let mut one = q();
        one.limit = 1;
        let first = read_child_page(s, "main", &one).await.unwrap();
        one.cursor = first.next_cursor;
        assert!(read_child_page(s, "main", &one).await.is_ok());
        // Provenance must count against the encoded quota, including cursors.
        one.cursor = None;
        one.max_bytes = size(&read_child_page(s, "main", &one).await.unwrap()).unwrap() - 1;
        assert!(read_child_page(s, "main", &one).await.is_err());
    }

    #[tokio::test]
    async fn native_memory_tree_fork_reads_copies_without_source() {
        use crate::session::memory::InMemorySessionRepo;
        let repo =
            InMemorySessionRepo::new(Arc::new(SystemClock), Arc::new(DefaultIdGenerator::new()));
        let source = repo.create(&SessionCreateOptions::default()).await.unwrap();
        let s = Session::new(Arc::new(source), None);
        let j = setup(&s).await;
        let args = json!({"owned": "x".repeat(70000)});
        let a = begin(&j, "a", "root-call", args.clone()).await;
        a.finish(&AgentToolResult::text("done"), false, false)
            .await
            .unwrap();
        a.close(None).await.unwrap();
        let child = a.id().to_owned();
        let phases = read_child_phase_page(&s, "main", &pq(&child))
            .await
            .unwrap();
        let mut foreign_body = bq(&child, &phases.phases[0]);
        foreign_body.cursor = read_child_body_page(&s, "main", &foreign_body)
            .await
            .unwrap()
            .next_cursor;
        let mut foreign_children = q();
        foreign_children.limit = 1;
        foreign_children.cursor = read_child_page(&s, "main", &foreign_children)
            .await
            .unwrap()
            .next_cursor;
        let metadata = s.get_metadata().await.unwrap();
        let copied = repo
            .fork(
                &metadata,
                &SessionCreateOptions::default(),
                &ForkOptions::Tree,
            )
            .await
            .unwrap();
        let copied = Session::new(Arc::new(copied), None);
        assert!(read_child_page(&copied, "main", &foreign_children)
            .await
            .is_err());
        assert!(read_child_body_page(&copied, "main", &foreign_body)
            .await
            .is_err());
        drop(a);
        drop(j);
        drop(s);
        repo.delete(&metadata).await.unwrap();
        assert!(repo.open(&metadata).await.is_err());
        assert_copy(&copied, &child, &serde_json::to_vec(&args).unwrap()).await;
    }

    #[tokio::test]
    async fn checkpoint_absence_partial_forgery_and_fork_metadata_fail_closed() {
        let store = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "source".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        let s = Session::new(store.clone(), None);
        let j = setup(&s).await;
        let a = begin(&j, "a", "root-call", json!({})).await;
        let child = a.id().to_owned();
        let requested = read_child_phase_page(&s, "main", &pq(&child))
            .await
            .unwrap()
            .phases
            .remove(0);
        let records = s
            .storage()
            .find_records(&RecordQuery::default())
            .await
            .unwrap();
        let with_checkpoint = InMemorySessionStorage::from_state(
            SessionMetadata {
                id: "destination".into(),
                created_at: 0,
                parent_session_id: Some("source".into()),
            },
            store.snapshot_state(),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        );
        let with_checkpoint = Session::new(Arc::new(with_checkpoint), None);
        assert_eq!(
            read_child_page(&with_checkpoint, "main", &q())
                .await
                .unwrap()
                .children[0]
                .origin_verification,
            ChildOriginVerification::NativeCheckpoint
        );
        let mut duplicate = records
            .iter()
            .find(|r| matches!(r, LaneRecord::ToolStarted(_)))
            .unwrap()
            .clone();
        if let LaneRecord::ToolStarted(start) = &mut duplicate {
            start.base.id = "duplicate-start".into();
        }
        with_checkpoint
            .storage()
            .append_record(duplicate)
            .await
            .unwrap();
        assert!(read_child_page(&with_checkpoint, "main", &q())
            .await
            .is_err());
        for parent in [
            None,
            Some("destination"),
            Some(""),
            Some("bad\nparent"),
            Some("source"),
        ] {
            for evidence in 0..4 {
                let copy = store
                    .fork_from(
                        SessionMetadata {
                            id: "destination".into(),
                            created_at: 0,
                            parent_session_id: parent.map(str::to_owned),
                        },
                        &ForkOptions::Tree,
                        Arc::new(SystemClock),
                        Arc::new(DefaultIdGenerator::new()),
                    )
                    .unwrap();
                let copy = Session::new(Arc::new(copy), None);
                for record in &records {
                    let wanted = match record {
                        LaneRecord::OperationStarted(_) => evidence == 1 || evidence == 3,
                        LaneRecord::ToolStarted(_) => evidence == 2 || evidence == 3,
                        _ => false,
                    };
                    if wanted {
                        copy.storage().append_record(record.clone()).await.unwrap();
                    }
                }
                // Appended partial/forged evidence must not downgrade to a copy.
                let valid = parent == Some("source") && evidence == 0;
                assert_eq!(
                    read_child_page(&copy, "main", &q()).await.is_ok(),
                    valid,
                    "{parent:?}/{evidence}"
                );
                assert_eq!(
                    read_child_phase_page(&copy, "main", &pq(&child))
                        .await
                        .is_ok(),
                    valid
                );
                assert_eq!(
                    read_child_body_page(&copy, "main", &bq(&child, &requested))
                        .await
                        .is_ok(),
                    valid
                );
            }
        }
    }

    #[tokio::test]
    async fn historical_copy_still_validates_native_begin_and_parent_authority() {
        let store = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "source".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        let s = Session::new(store.clone(), None);
        let j = setup(&s).await;
        let a = begin(&j, "a", "root-call", json!({})).await;
        let original = exact(&s, a.id(), a.id(), Some(BEGIN_CUSTOM_TYPE))
            .await
            .unwrap();
        for mutation in 0..7 {
            let copy = store
                .fork_from(
                    SessionMetadata {
                        id: "copy".into(),
                        created_at: 0,
                        parent_session_id: Some("source".into()),
                    },
                    &ForkOptions::Tree,
                    Arc::new(SystemClock),
                    Arc::new(DefaultIdGenerator::new()),
                )
                .unwrap();
            let copy = Session::new(Arc::new(copy), None);
            let mut value =
                serde_json::to_value(parse::<Begin>(&original, BEGIN_CUSTOM_TYPE).unwrap())
                    .unwrap();
            let new_id = if mutation == 6 {
                "tpchild-not-a-uuid".into()
            } else {
                format!("tpchild-{}", Uuid::new_v4())
            };
            value["childId"] = json!(new_id);
            match mutation {
                0 => value["origin"]["toolIndex"] = json!(1),
                1 => value["version"] = json!(2),
                2 => value["parentChildId"] = json!(new_id),
                3 => value["origin"]["assistantEntryId"] = json!(a.id()),
                4 => value["parentChildId"] = json!(format!("tpchild-{}", Uuid::new_v4())),
                5 => value["unexpected"] = json!(true),
                _ => {}
            }
            raw(&copy, new_id.clone(), BEGIN_CUSTOM_TYPE, value).await;
            assert!(
                read_child_page(&copy, "main", &q()).await.is_err(),
                "mutation {mutation}"
            );
            assert!(read_child_phase_page(&copy, "main", &pq(&new_id))
                .await
                .is_err());
        }
        let copy = store
            .fork_from(
                SessionMetadata {
                    id: "copy".into(),
                    created_at: 0,
                    parent_session_id: Some("x".repeat(513)),
                },
                &ForkOptions::Tree,
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )
            .unwrap();
        assert!(
            read_child_page(&Session::new(Arc::new(copy), None), "main", &q())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_jsonl_tree_branch_forks_are_destination_owned() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| {
            dir.path()
                .join(format!("{name}.jsonl"))
                .to_string_lossy()
                .replace(char::from(92), "/")
        };
        let fs: Arc<dyn rpi_tools::FileSystem> =
            Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into()));
        let clock = Arc::new(SystemClock);
        let ids = Arc::new(DefaultIdGenerator::new());
        let header =
            |name: &str, parent| JsonlV4Header::new(name.into(), 1, "/".into(), parent, None, None);
        let source = Arc::new(
            JsonlSessionStorage::create(
                fs.clone(),
                &path("source"),
                header("source", None),
                clock.clone(),
                ids.clone(),
            )
            .await
            .unwrap(),
        );
        let s = Session::new(source.clone(), None);
        let j = setup(&s).await;
        let args = json!({"copied": "y".repeat(70000)});
        let a = begin(&j, "a", "root-call", args.clone()).await;
        a.finish(&AgentToolResult::text("done"), false, false)
            .await
            .unwrap();
        a.close(None).await.unwrap();
        let child = a.id().to_owned();
        let result = s
            .append_message(
                rpi_ai::types::ToolResultMessage {
                    role: Default::default(),
                    tool_call_id: "root-call".into(),
                    tool_name: "program".into(),
                    content: AgentToolResult::text("done").into_content(),
                    details: None,
                    usage: None,
                    added_tool_names: vec![],
                    is_error: false,
                    timestamp: 1,
                }
                .into(),
            )
            .await
            .unwrap();
        let future = begin(&j, "future", "root-call", json!({"future": true})).await;
        let future_id = future.id().to_owned();
        let phases = read_child_phase_page(&s, "main", &pq(&child))
            .await
            .unwrap();
        let requested = &phases.phases[0];
        let descriptor = requested.descriptor.as_ref().unwrap();
        let mut foreign = bq(&child, requested);
        foreign.cursor = read_child_body_page(&s, "main", &foreign)
            .await
            .unwrap()
            .next_cursor;
        let mut foreign_phase = pq(&child);
        foreign_phase.limit = 1;
        foreign_phase.cursor = read_child_phase_page(&s, "main", &foreign_phase)
            .await
            .unwrap()
            .next_cursor;
        for (name, options) in [
            ("tree", ForkOptions::Tree),
            (
                "branch",
                ForkOptions::Branch {
                    entry_id: Some(result),
                    position: Some(ForkPosition::At),
                },
            ),
            (
                "early",
                ForkOptions::Branch {
                    entry_id: Some("assistant".into()),
                    position: Some(ForkPosition::At),
                },
            ),
            ("missing-body", ForkOptions::Tree),
            ("missing-descriptor", ForkOptions::Tree),
        ] {
            let store = source
                .fork(
                    &path(name),
                    header(name, Some("source".into())),
                    &options,
                    clock.clone(),
                    ids.clone(),
                )
                .await
                .unwrap();
            store.drain().await;
            drop(store);
        }
        // Source remains available: missing destination evidence cannot fall back.
        for (name, target, typ) in [
            (
                "missing-body",
                format!("tpbody-{}-chunk-0", descriptor.body_id),
                tool_program_body::CHUNK_CUSTOM_TYPE,
            ),
            (
                "missing-descriptor",
                descriptor.descriptor_id.clone(),
                COMMIT_CUSTOM_TYPE,
            ),
        ] {
            let file = path(name);
            let text = std::fs::read_to_string(&file).unwrap();
            let mut changed = false;
            let rewritten = text
                .lines()
                .map(|line| {
                    let mut value: Value = serde_json::from_str(line).unwrap();
                    if value["id"].as_str() == Some(target.as_str()) {
                        assert_eq!(value["customType"], typ);
                        value["customType"] = json!("unrelated");
                        changed = true;
                    }
                    serde_json::to_string(&value).unwrap() + "\n"
                })
                .collect::<String>();
            assert!(changed);
            std::fs::write(&file, rewritten).unwrap();
            let store = JsonlSessionStorage::load(fs.clone(), &file, clock.clone(), ids.clone())
                .await
                .unwrap();
            let copied = Session::new(Arc::new(store), None);
            assert!(
                read_child_body_page(&copied, "main", &bq(&child, requested))
                    .await
                    .is_err()
            );
            if name == "missing-descriptor" {
                assert!(read_child_phase_page(&copied, "main", &pq(&child))
                    .await
                    .is_err());
            }
        }
        drop(a);
        drop(future);
        drop(j);
        s.drain().await;
        drop(s);
        drop(source);
        std::fs::remove_file(path("source")).unwrap();
        std::fs::remove_file(format!("{}.progress.jsonl", path("source"))).unwrap();
        for name in ["tree", "branch", "early"] {
            let store =
                JsonlSessionStorage::load(fs.clone(), &path(name), clock.clone(), ids.clone())
                    .await
                    .unwrap();
            let copied = Session::new(Arc::new(store), None);
            assert!(read_child_body_page(&copied, "main", &foreign)
                .await
                .is_err());
            assert!(read_child_phase_page(&copied, "main", &foreign_phase)
                .await
                .is_err());
            if name == "early" {
                assert!(read_child_page(&copied, "main", &q())
                    .await
                    .unwrap()
                    .children
                    .is_empty());
                assert!(read_child_phase_page(&copied, "main", &pq(&child))
                    .await
                    .is_err());
                assert!(
                    read_child_body_page(&copied, "main", &bq(&child, requested))
                        .await
                        .is_err()
                );
            } else {
                assert_copy(&copied, &child, &serde_json::to_vec(&args).unwrap()).await;
                assert_eq!(
                    read_child_page(&copied, "main", &q())
                        .await
                        .unwrap()
                        .children
                        .len(),
                    if name == "tree" { 2 } else { 1 }
                );
                if name == "branch" {
                    assert!(read_child_phase_page(&copied, "main", &pq(&future_id))
                        .await
                        .is_err());
                }
            }
        }
    }

    #[tokio::test]
    async fn cold_jsonl_reopen_does_not_hydrate_unrelated_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("children.jsonl")
            .to_string_lossy()
            .replace('\\', "/");
        let fs: Arc<dyn rpi_tools::FileSystem> =
            Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into()));
        let clock = Arc::new(SystemClock);
        let ids = Arc::new(DefaultIdGenerator::new());
        let store = JsonlSessionStorage::create(
            fs.clone(),
            &path,
            JsonlV4Header::new("cold-queries".into(), 1, "/".into(), None, None, None),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        let s = Session::new(Arc::new(store), None);
        let j = setup(&s).await;
        let a = begin(&j, "a", "root-call", json!({"selected":"kept"})).await;
        let b = begin(&j, "b", "root-call", json!("UNRELATED".repeat(12000))).await;
        let other = read_child_phase_page(&s, "main", &pq(b.id()))
            .await
            .unwrap()
            .phases[0]
            .descriptor
            .clone()
            .unwrap();
        let target = a.id().to_owned();
        drop(a);
        drop(b);
        drop(j);
        s.drain().await;
        drop(s);
        let store = JsonlSessionStorage::load(fs, &path, clock, ids)
            .await
            .unwrap();
        let s = Session::new(Arc::new(store), None);
        let raw = std::fs::read_to_string(&path).unwrap();
        let needle = format!("tpbody-{}-chunk-0", other.body_id);
        let corrupt = raw.replacen(&needle, &needle.replace("chunk-0", "chunk-X"), 1);
        assert_ne!(raw, corrupt);
        std::fs::write(&path, corrupt).unwrap();
        assert_eq!(
            read_child_page(&s, "main", &q())
                .await
                .unwrap()
                .children
                .len(),
            2
        );
        let phases = read_child_phase_page(&s, "main", &pq(&target))
            .await
            .unwrap();
        assert_eq!(phases.phases.len(), 1);
        assert_eq!(phases.state, ChildState::Unknown);
        assert!(
            read_child_body_page(&s, "main", &bq(&target, &phases.phases[0]))
                .await
                .is_ok()
        );
        let mut foreign = pq(&target);
        foreign.cursor = Some(ChildCursor {
            session_id: "source-session".into(),
            lane: "main".into(),
            leaf_id: phases.leaf_id,
            direction: Direction::OldestFirst,
            child_id: Some(target),
            after_entry_id: phases.phases[0].entry_id.clone(),
            after_seq: phases.phases[0].seq,
        });
        assert!(read_child_phase_page(&s, "main", &foreign).await.is_err());
    }
}
