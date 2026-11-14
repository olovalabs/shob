//! Transactional SQL mirrors and digest-bound navigation/UI metadata. Not a
//! proof of global index completeness against malicious external SQL rewriting.
use super::facts::types::*;
use super::{
    engine::{self, ensure, Effect, Identity, Meta, Result},
    selection::Selection,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub(super) fn schema(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE selection(seq INTEGER PRIMARY KEY,kind TEXT NOT NULL,lane TEXT,keys TEXT NOT NULL);
        CREATE INDEX select_entry_type ON selection(json_extract(keys,'$.entry_type'),seq);
        CREATE INDEX select_custom_type ON selection(json_extract(keys,'$.custom_type'),seq);
        CREATE INDEX select_operation ON selection(json_extract(keys,'$.operation'),seq);
        CREATE INDEX select_record_type ON selection(json_extract(keys,'$.record_type'),lane,seq);
        CREATE INDEX select_lane ON selection(lane,seq);
        CREATE INDEX select_run ON selection(json_extract(keys,'$.run'),seq);
        CREATE INDEX select_queue ON selection(json_extract(keys,'$.queue'),seq);
        CREATE INDEX select_target ON selection(json_extract(keys,'$.target'),seq);
        CREATE INDEX select_outbox ON selection(json_extract(keys,'$.outbox'),seq);
        CREATE TABLE tool_keys(tool TEXT NOT NULL,seq INTEGER NOT NULL,PRIMARY KEY(tool,seq));
        CREATE INDEX select_tool_seq ON tool_keys(seq,tool);
        CREATE TABLE navigation(seq INTEGER PRIMARY KEY,json TEXT NOT NULL,digest BLOB NOT NULL);
        CREATE TABLE ui_summary(seq INTEGER PRIMARY KEY,json TEXT NOT NULL,digest BLOB NOT NULL);")?;
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Links {
    pub seq: u64,
    pub parent: Option<u64>,
    pub nearest_user: Option<u64>,
    pub previous_user: Option<u64>,
    pub nearest_transcript: Option<u64>,
    pub previous_transcript: Option<u64>,
}
fn publish_json(
    db: &Connection,
    identity: &Identity,
    seq: u64,
    table: &str,
    domain: &str,
    value: &impl Serialize,
) -> Result<()> {
    let json = serde_json::to_string(value)?;
    ensure(json.len() <= 4096, "navigation/summary budget exceeded")?;
    // table is a static internal identifier, never user input.
    db.execute(
        &format!("INSERT INTO {table} VALUES(?1,?2,?3)"),
        params![
            seq,
            json,
            engine::digest(identity, seq, domain, json.as_bytes())
        ],
    )?;
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(
    db: &Connection,
    identity: &Identity,
    seq: u64,
    table: &str,
    domain: &str,
) -> Result<T> {
    let(json,hash):(Option<String>,Option<Vec<u8>>)=db.query_row(&format!("SELECT CASE WHEN length(CAST(json AS BLOB))<=4096 THEN json END,CASE WHEN length(digest)=32 THEN digest END FROM {table} WHERE seq=?1"),[seq],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let json = json.ok_or("navigation/summary budget exceeded")?;
    ensure(
        hash.as_deref() == Some(engine::digest(identity, seq, domain, json.as_bytes()).as_slice()),
        "navigation/summary digest mismatch",
    )?;
    Ok(serde_json::from_str(&json)?)
}
fn summary(entry: &Entry) -> Option<MessageSummary> {
    let Entry::Message(message) = entry else {
        return None;
    };
    let rpi_agent::AgentMessage::User(user) = &message.message else {
        return None;
    };
    let (preview, has_images) = super::preview::preview(&user.content);
    Some(MessageSummary {
        entry_id: entry.id().into(),
        role: "user".into(),
        preview,
        timestamp: user.timestamp,
        has_images,
    })
}
pub(super) fn publish(
    db: &Connection,
    identity: &Identity,
    meta: &Meta,
    mutation: &SessionMutation,
) -> Result<()> {
    db.execute(
        "INSERT INTO selection VALUES(?1,?2,?3,?4)",
        params![
            meta.seq,
            meta.kind(),
            meta.lane(),
            serde_json::to_string(&meta.selection)?
        ],
    )?;
    for tool in meta.selection.tools.iter().collect::<BTreeSet<_>>() {
        db.execute(
            "INSERT INTO tool_keys VALUES(?1,?2)",
            params![tool, meta.seq],
        )?;
    }
    if let SessionMutation::Entry { entry, .. } = mutation {
        let parent = match entry.parent_id() {
            Some(id) => Some(db.query_row(
                "SELECT seq FROM facts WHERE id=?1 AND category='entry' AND seq<?2",
                params![id, meta.seq],
                |r| r.get::<_, u64>(0),
            )?),
            None => None,
        };
        let inherited = parent
            .map(|seq| read_links(db, identity, seq))
            .transpose()?;
        let previous_user = inherited.as_ref().and_then(|l| l.nearest_user);
        let previous_transcript = inherited.as_ref().and_then(|l| l.nearest_transcript);
        let links = Links {
            seq: meta.seq,
            parent,
            previous_user,
            previous_transcript,
            nearest_user: if meta.selection.user {
                Some(meta.seq)
            } else {
                previous_user
            },
            nearest_transcript: if meta.selection.transcript {
                Some(meta.seq)
            } else {
                previous_transcript
            },
        };
        publish_json(db, identity, meta.seq, "navigation", "navigation", &links)?;
        if let Some(summary) = summary(entry) {
            publish_json(db, identity, meta.seq, "ui_summary", "ui-summary", &summary)?;
        }
    }
    Ok(())
}
pub(super) fn validate_selection(db: &Connection, meta: &Meta) -> Result<()> {
    let(kind,lane,json):(String,Option<String>,Option<String>)=db.query_row("SELECT CASE WHEN length(CAST(kind AS BLOB))<=32 THEN kind ELSE '' END,CASE WHEN length(CAST(lane AS BLOB))<=16384 THEN lane END,CASE WHEN length(CAST(keys AS BLOB))<=16384 THEN keys END FROM selection WHERE seq=?1",[meta.seq],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let selection: Selection = serde_json::from_str(&json.ok_or("selection budget exceeded")?)?;
    ensure(
        kind == meta.kind() && lane.as_deref() == meta.lane() && selection == meta.selection,
        "selection/metadata mismatch",
    )?;
    let expected = meta
        .selection
        .tools
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let maximum = expected.iter().map(|tool| tool.len()).max().unwrap_or(0);
    let mut statement=db.prepare("SELECT CASE WHEN length(CAST(tool AS BLOB))<=?3 THEN tool END FROM tool_keys WHERE seq=?1 ORDER BY tool LIMIT ?2")?;
    let mut rows = statement.query(params![meta.seq, expected.len() + 1, maximum])?;
    for expected in expected {
        let row = rows.next()?.ok_or("tool index/metadata mismatch")?;
        ensure(
            matches!(row.get_ref(0)?,rusqlite::types::ValueRef::Text(actual) if actual==expected.as_bytes()),
            "tool index/metadata mismatch",
        )?;
    }
    ensure(rows.next()?.is_none(), "tool index/metadata mismatch")?;
    Ok(())
}
fn raw_links(db: &Connection, identity: &Identity, seq: u64) -> Result<Links> {
    let links: Links = read_json(db, identity, seq, "navigation", "navigation")?;
    ensure(links.seq == seq && seq > 0, "invalid navigation identity")?;
    for previous in [links.parent, links.previous_user, links.previous_transcript]
        .into_iter()
        .flatten()
    {
        ensure(previous > 0 && previous < seq, "invalid navigation order")?;
    }
    for nearest in [links.nearest_user, links.nearest_transcript]
        .into_iter()
        .flatten()
    {
        ensure(
            nearest > 0 && nearest <= seq,
            "invalid nearest reference order",
        )?;
    }
    Ok(links)
}
pub(super) fn read_links(db: &Connection, identity: &Identity, seq: u64) -> Result<Links> {
    let links = raw_links(db, identity, seq)?;
    let meta = engine::read_metadata(db, identity, seq)?.0;
    let Effect::Entry { parent, .. } = meta.effect else {
        return Err("navigation must reference entry".into());
    };
    let inherited = match (parent, links.parent) {
        (None, None) => None,
        (Some(id), Some(parent)) => {
            let p = engine::read_metadata(db, identity, parent)?.0;
            ensure(
                p.id() == Some(id.as_str()) && matches!(p.effect, Effect::Entry { .. }),
                "navigation parent mismatch",
            )?;
            let inherited = raw_links(db, identity, parent)?;
            validate_nearest(&inherited, &p.selection)?;
            Some(inherited)
        }
        _ => return Err("navigation parent mismatch".into()),
    };
    ensure(
        links.previous_user == inherited.as_ref().and_then(|p| p.nearest_user)
            && links.previous_transcript == inherited.as_ref().and_then(|p| p.nearest_transcript),
        "navigation inheritance mismatch",
    )?;
    validate_nearest(&links, &meta.selection)?;
    Ok(links)
}
fn validate_nearest(links: &Links, selection: &Selection) -> Result<()> {
    ensure(
        links.nearest_user
            == if selection.user {
                Some(links.seq)
            } else {
                links.previous_user
            }
            && links.nearest_transcript
                == if selection.transcript {
                    Some(links.seq)
                } else {
                    links.previous_transcript
                },
        "nearest navigation type mismatch",
    )
}
pub(super) fn read_summary(
    db: &Connection,
    identity: &Identity,
    seq: u64,
) -> Result<MessageSummary> {
    let meta = engine::read_metadata(db, identity, seq)?.0;
    ensure(meta.selection.user, "summary must reference user message")?;
    let summary: MessageSummary = read_json(db, identity, seq, "ui_summary", "ui-summary")?;
    ensure(
        meta.id() == Some(summary.entry_id.as_str())
            && summary.role == "user"
            && summary.preview.chars().count() <= 140,
        "invalid user summary",
    )?;
    Ok(summary)
}
pub(super) fn verify_payload(
    db: &Connection,
    identity: &Identity,
    meta: &Meta,
    mutation: &SessionMutation,
) -> Result<()> {
    if let SessionMutation::Entry { entry, .. } = mutation {
        read_links(db, identity, meta.seq)?;
        if let Some(expected) = summary(entry) {
            let actual = read_summary(db, identity, meta.seq)?;
            ensure(
                serde_json::to_value(actual)? == serde_json::to_value(expected)?,
                "summary/body mismatch",
            )?;
        }
    }
    Ok(())
}

pub(super) fn key_for_id(db: &Connection, id: &str) -> Result<Option<u64>> {
    Ok(db
        .query_row(
            "SELECT seq FROM facts WHERE id=?1 AND category='entry'",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}
