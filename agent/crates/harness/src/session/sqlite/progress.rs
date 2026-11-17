use super::*;
use crate::session::types::{AssistantFrameOp, AssistantFrameRecord};

fn failure(error: impl std::fmt::Display) -> SessionError {
    SessionError::storage(format!("SQLite progress: {error}"))
}

impl SqliteSessionStorage {
    pub(super) async fn prune_completed_progress_sql(&self) -> SessionResult<usize> {
        let _gate = self.gate.lock().await;
        let db = self.db.lock().map_err(failure)?;
        // One authoritative database statement: no stale Memory mirror, no
        // progress-row-times-history scan, and no state_json rewrite.
        db.execute(
            "WITH finished AS MATERIALIZED (
                SELECT json_extract(r.value, '$.lane') AS lane,
                       json_extract(r.value, '$.runId') AS run_id
                FROM sessions AS s, json_each(s.state_json, '$.records') AS r
                WHERE s.id = ?1 AND json_extract(r.value, '$.type') = 'operation_finished'
             )
             DELETE FROM progress_frames WHERE session_id = ?1
             AND (lane, run_id) IN (SELECT lane, run_id FROM finished)",
            params![self.inner.metadata().id],
        )
        .map_err(failure)
    }

    pub(super) async fn append_progress_sql(
        &self,
        records: Vec<AssistantFrameRecord>,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        let _gate = self.gate.lock().await;
        let mut db = self.db.lock().map_err(failure)?;
        let transaction = db.transaction().map_err(failure)?;
        let session_id = self.inner.metadata().id;
        for record in &records {
            let found: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM sessions, json_each(sessions.state_json, '$.lanes') WHERE sessions.id = ?1 AND json_each.key = ?2)",params![session_id,record.base.lane],|row| row.get(0)).map_err(failure)?;
            if !found {
                return Err(SessionError::invalid_payload(
                    "unknown progress lane/session",
                ));
            }
            let duplicate: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM progress_frames WHERE session_id=?1 AND id=?2)",
                    params![session_id, record.base.id],
                    |row| row.get(0),
                )
                .map_err(failure)?;
            if duplicate {
                return Err(SessionError::invalid_payload("duplicate progress id"));
            }
        }
        transaction
            .execute(
                "INSERT OR IGNORE INTO progress_heads(session_id, sequence) VALUES (?1, 0)",
                params![session_id],
            )
            .map_err(failure)?;
        let sequence: u64 = transaction
            .query_row(
                "SELECT sequence FROM progress_heads WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(failure)?;
        let state = crate::session::progress::ProgressState::at_sequence(sequence);
        let stamped = state.prepare(records, || {
            crate::session::memory::Clock::now_ms(&SystemClock)
        })?;
        for record in &stamped {
            match record.op {
                AssistantFrameOp::Append => {
                    let json = serde_json::to_string(record).map_err(failure)?;
                    transaction.execute("INSERT INTO progress_frames(session_id, sequence, id, lane, run_id, stream_index, frame_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",params![session_id,record.base.seq,record.base.id,record.base.lane,record.run_id,record.stream_index as u64,json]).map_err(failure)?;
                }
                AssistantFrameOp::ClearStream => {
                    transaction.execute("DELETE FROM progress_frames WHERE session_id=?1 AND lane=?2 AND run_id=?3 AND stream_index=?4",params![session_id,record.base.lane,record.run_id,record.stream_index as u64]).map_err(failure)?;
                }
                AssistantFrameOp::ClearRun => {
                    transaction.execute("DELETE FROM progress_frames WHERE session_id=?1 AND lane=?2 AND run_id=?3",params![session_id,record.base.lane,record.run_id]).map_err(failure)?;
                }
                AssistantFrameOp::CommitStream => unreachable!(),
            }
        }
        if let Some(last) = stamped.last() {
            transaction
                .execute(
                    "UPDATE progress_heads SET sequence=?1 WHERE session_id=?2",
                    params![last.base.seq, session_id],
                )
                .map_err(failure)?;
        }
        transaction.commit().map_err(failure)?;
        Ok(stamped)
    }
    pub(super) async fn read_progress_sql(
        &self,
        run_id: &str,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        let _gate = self.gate.lock().await;
        let db = self.db.lock().map_err(failure)?;
        let mut statement = db.prepare("SELECT frame_json FROM progress_frames WHERE session_id=?1 AND run_id=?2 ORDER BY sequence").map_err(failure)?;
        let rows = statement
            .query_map(params![self.inner.metadata().id, run_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(failure)?;
        rows.map(|row| serde_json::from_str(&row.map_err(failure)?).map_err(failure))
            .collect()
    }
}
