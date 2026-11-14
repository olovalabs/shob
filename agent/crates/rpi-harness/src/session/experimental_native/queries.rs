//! SQL selects bounded keys before validating metadata and loading selected bodies.
use super::{
    engine::{self, Effect, Identity, Result},
    facts::types::*,
};
use crate::error::SessionError;
use rusqlite::{params_from_iter, types::Value, Connection};

struct Query {
    sql: String,
    values: Vec<Value>,
}
impl Query {
    fn new(predicate: &str) -> Self {
        Self {
            sql: predicate.into(),
            values: Vec::new(),
        }
    }
    fn text(&mut self, expression: &str, value: &str) {
        self.sql.push_str(&format!(" AND {expression}=?"));
        self.values.push(Value::Text(value.into()));
    }
    fn sequence(&mut self, column: &str, greater: bool, value: u64) {
        if value > i64::MAX as u64 {
            if greater {
                self.sql.push_str(" AND 0");
            }
        } else {
            self.sql.push_str(&format!(
                " AND {column}{}?",
                if greater { ">" } else { "<" }
            ));
            self.values.push(Value::Integer(value as i64));
        }
    }
    fn keys(mut self, db: &Connection, descending: bool, limit: Option<usize>) -> Result<Vec<u64>> {
        if limit == Some(0) {
            return Err(SessionError::invalid_query("limit must be positive").into());
        }
        self.sql=format!("SELECT f.seq FROM facts f JOIN selection s ON s.seq=f.seq WHERE {} ORDER BY f.seq {} LIMIT ?",self.sql,if descending{"DESC"}else{"ASC"});
        self.values.push(Value::Integer(
            limit.map(|v| v.min(i64::MAX as usize) as i64).unwrap_or(-1),
        ));
        let mut statement = db.prepare(&self.sql)?;
        let keys = statement
            .query_map(params_from_iter(self.values), |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(keys)
    }
}
fn tool(query: &mut Query, id: &str) {
    query
        .sql
        .push_str(" AND EXISTS(SELECT 1 FROM tool_keys t WHERE t.seq=f.seq AND t.tool=?)");
    query.values.push(Value::Text(id.into()));
}
pub(super) fn entries(db: &Connection, identity: &Identity, q: &EntryQuery) -> Result<Vec<Entry>> {
    engine::restore(db, identity)?;
    let mut query = Query::new("f.category='entry'");
    if let Some(value) = q.entry_type {
        query.text("json_extract(s.keys,'$.entry_type')", value);
    }
    if let Some(value) = &q.custom_type {
        query.text("json_extract(s.keys,'$.custom_type')", value);
    }
    if q.exclude_tool_program_bodies {
        query
            .sql
            .push_str(" AND (COALESCE(json_extract(s.keys,'$.custom_type'),'') NOT IN (?,?))");
        for custom_type in [
            crate::tool_program_body::CHUNK_CUSTOM_TYPE,
            crate::tool_program_body::COMMIT_CUSTOM_TYPE,
        ] {
            query.values.push(Value::Text(custom_type.into()));
        }
    }
    if let Some(value) = &q.entry_id {
        query.text("f.id", value);
    }
    if let Some(value) = &q.outbox_key {
        query.text("json_extract(s.keys,'$.outbox')", value);
    }
    if let Some(value) = &q.tool_call_id {
        tool(&mut query, value);
    }
    if q.message_boundary_only {
        query
            .sql
            .push_str(" AND json_extract(s.keys,'$.boundary')=1");
    }
    if let Some(cursor) = q.cursor {
        query.sequence(
            "f.seq",
            q.order == Some(EntryOrder::OldestFirst),
            cursor.after_seq,
        );
    }
    query.keys(db,q.order!=Some(EntryOrder::OldestFirst),q.limit)?.into_iter().map(|seq|{
        let meta=engine::read_metadata(db,identity,seq)?.0;
        engine::ensure(matches!(meta.effect,Effect::Entry{ref id,..}if meta.selection.entry_matches(id,seq,q)),"entry SQL selection mismatch")?;
        entry(db,identity,seq)
    }).collect()
}
pub(super) fn entry(db: &Connection, identity: &Identity, seq: u64) -> Result<Entry> {
    match engine::read_mutation(db, identity, seq)? {
        SessionMutation::Entry { entry, .. } => Ok(entry),
        _ => Err("invalid entry selection".into()),
    }
}
fn record_query(q: &RecordQuery) -> Result<Query> {
    let mut query = Query::new("f.category IN ('record','finish','cancel')");
    if let Some(value) = &q.lane {
        query.text("s.lane", value);
    }
    if let Some(value) = q.record_type {
        query.text("json_extract(s.keys,'$.record_type')", value);
    }
    if let Some(value) = &q.run_id {
        query.text("json_extract(s.keys,'$.run')", value);
    }
    if let Some(value) = q.operation_kind {
        query.text("json_extract(s.keys,'$.operation')", value);
    }
    if let Some(value) = q.queue_kind {
        let value = serde_json::to_value(value)?;
        query.text(
            "json_extract(s.keys,'$.queue')",
            value.as_str().ok_or("invalid queue key")?,
        );
    }
    if let Some(value) = &q.target_entry_id {
        query.text("json_extract(s.keys,'$.target')", value);
    }
    if let Some(value) = &q.tool_call_id {
        query.text("json_extract(s.keys,'$.record_type')", "tool_started");
        tool(&mut query, value);
    }
    if let Some(value) = q.after_seq {
        query.sequence("f.seq", true, value);
    }
    Ok(query)
}
pub(super) fn records(
    db: &Connection,
    identity: &Identity,
    q: &RecordQuery,
) -> Result<Vec<LaneRecord>> {
    engine::restore(db, identity)?;
    record_query(q)?.keys(db,q.order!=Some(EntryOrder::OldestFirst),q.limit)?.into_iter().map(|seq|{
        let meta=engine::read_metadata(db,identity,seq)?.0;
        engine::ensure(matches!(meta.effect,Effect::Record{ref lane,..} if meta.selection.record_matches(lane,seq,q)),"record SQL selection mismatch")?;
        record(db,identity,seq)
    }).collect()
}
fn record(db: &Connection, identity: &Identity, seq: u64) -> Result<LaneRecord> {
    match engine::read_mutation(db, identity, seq)? {
        SessionMutation::Record { record } => Ok(record),
        _ => Err("invalid record selection".into()),
    }
}
pub(super) fn page(
    db: &Connection,
    identity: &Identity,
    q: &RecordPageQuery,
) -> Result<RecordPage> {
    if q.limit == 0 {
        return Err(SessionError::invalid_query("limit must be positive").into());
    }
    engine::restore(db, identity)?;
    let mut query = record_query(&RecordQuery {
        lane: Some(q.lane.clone()),
        after_seq: q.after_seq,
        ..Default::default()
    })?;
    if let Some(before) = q.before_seq {
        query.sequence("f.seq", false, before);
    }
    let total: u64 = db.query_row(
        &format!(
            "SELECT COUNT(*) FROM facts f JOIN selection s ON s.seq=f.seq WHERE {}",
            query.sql
        ),
        params_from_iter(query.values.iter()),
        |r| r.get(0),
    )?;
    let mut keys = query.keys(db, q.from_end, Some(q.limit))?;
    if q.from_end {
        keys.reverse();
    }
    let records = keys
        .into_iter()
        .map(|seq| {
            let meta = engine::read_metadata(db, identity, seq)?.0;
            engine::ensure(
                matches!(meta.effect,Effect::Record{ref lane,..}if *lane==q.lane)
                    && q.after_seq.is_none_or(|v| seq > v)
                    && q.before_seq.is_none_or(|v| seq < v),
                "record page selection mismatch",
            )?;
            record(db, identity, seq)
        })
        .collect::<Result<_>>()?;
    Ok(RecordPage {
        records,
        total: usize::try_from(total)?,
    })
}
pub(super) fn log_keys(
    db: &Connection,
    identity: &Identity,
    options: &LogOptions,
) -> Result<Vec<u64>> {
    engine::restore(db, identity)?;
    let mut query = Query::new("1");
    if let Some(after) = options.after_seq {
        query.sequence("f.seq", true, after);
    }
    query.keys(db, false, options.limit)
}
