//! Strict nonrepairing saved view. No storage or file capability escapes.
use super::*;
use crate::session::types::{EntryCursor, EntryOrder};

pub struct SavedSessionReadQuery {
    pub session_id: String,
    pub limit: usize,
    pub after_seq: Option<u64>,
    pub custom_types: Vec<String>,
    pub deadline: std::time::Instant,
}
pub struct SavedSessionReadData {
    pub metadata: JsonlSessionMetadata,
    pub values: crate::session::SessionValues,
    pub leaf_id: Option<String>,
    /// Whether after_seq identifies a message on the canonical branch. Callers
    /// compare their fingerprint/leaf first so a changed branch wins over this.
    pub cursor_valid: bool,
    pub messages: Vec<Entry>,
    pub has_more: bool,
    pub custom_entries: Vec<Entry>,
    pub native_pending: bool,
    pub progress_pending: bool,
    pub idle_proof_supported: bool,
}
pub(super) fn deadline(at: std::time::Instant) -> SessionResult<()> {
    if std::time::Instant::now() >= at {
        Err(SessionError::invalid_entry(
            "saved read processing deadline exceeded",
        ))
    } else {
        Ok(())
    }
}
struct SizeLimit(usize);
impl std::io::Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("saved read custom entry byte limit exceeded"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl JsonlSessionStorage {
    /// Caller supplies a restricted read-only, globally byte-budgeted filesystem
    /// and retains the canonical noncreating observation lock. This strict cold
    /// replay never normalizes history/progress and returns data, not a writer.
    pub async fn read_saved_session(
        fs: Arc<dyn FileSystem>,
        path: &str,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        query: SavedSessionReadQuery,
    ) -> SessionResult<SavedSessionReadData> {
        if !(1..=128).contains(&query.limit) || query.custom_types.len() > 8 {
            return Err(SessionError::invalid_entry(
                "invalid saved read query limit",
            ));
        }
        let mut types = HashSet::new();
        if query.custom_types.iter().any(|s| {
            s.is_empty()
                || s == crate::session::values::VALUE_CUSTOM_TYPE
                || s.len() > 256
                || s.chars().any(char::is_control)
                || !types.insert(s.clone())
        }) {
            return Err(SessionError::invalid_entry(
                "invalid saved read custom types",
            ));
        }
        let storage = Self::load_impl(
            fs,
            path,
            clock,
            ids,
            ReadPolicy::SavedReadCold(query.deadline),
        )
        .await?;
        if storage.metadata.id != query.session_id {
            return Err(SessionError::invalid_entry(
                "saved session header identity mismatch",
            ));
        }
        if storage.state.lock().await.get_lanes().len() > 32 {
            return Err(SessionError::invalid_entry(
                "saved read lane limit exceeded",
            ));
        }
        deadline(query.deadline)?;
        let progress_pending = storage
            .progress
            .inspect_pending_without_repair_with_deadline(
                query.deadline,
                ReadPolicy::SAVED_MUTATION_LIMIT,
                ReadPolicy::SAVED_MUTATION_LIMIT,
            )
            .await?;
        let idle_proof_supported = storage.progress.required;
        let facts = storage.facts().await?;
        deadline(query.deadline)?;
        let session = crate::session::Session::new(Arc::new(storage), None);
        let leaf_id = session.get_leaf_id().await?;
        deadline(query.deadline)?;
        let cursor_valid = if let Some(after) = query.after_seq {
            if after == 0 {
                false
            } else {
                let matching = session
                    .find_entries_on_branch(
                        &EntryQuery {
                            entry_type: Some("message"),
                            order: Some(EntryOrder::OldestFirst),
                            cursor: Some(EntryCursor {
                                after_seq: after - 1,
                            }),
                            limit: Some(1),
                            ..Default::default()
                        },
                        &BranchBounds::default(),
                    )
                    .await?;
                matching.first().map(Entry::seq) == Some(after)
            }
        } else {
            true
        };
        deadline(query.deadline)?;
        let mut messages = session
            .find_entries_on_branch(
                &EntryQuery {
                    entry_type: Some("message"),
                    order: Some(EntryOrder::OldestFirst),
                    cursor: query.after_seq.map(|after_seq| EntryCursor { after_seq }),
                    limit: Some(query.limit + 1),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await?;
        deadline(query.deadline)?;
        let has_more = messages.len() > query.limit;
        messages.truncate(query.limit);
        let mut custom_entries = Vec::new();
        for custom_type in query.custom_types {
            deadline(query.deadline)?;
            custom_entries.extend(
                session
                    .find_entries_on_branch(
                        &EntryQuery {
                            entry_type: Some("custom"),
                            custom_type: Some(custom_type),
                            order: Some(EntryOrder::OldestFirst),
                            limit: Some(1025),
                            ..Default::default()
                        },
                        &BranchBounds::default(),
                    )
                    .await?,
            );
            if custom_entries.len() > 1024 {
                return Err(SessionError::invalid_entry(
                    "saved read custom entry count limit exceeded",
                ));
            }
        }
        custom_entries.sort_by_key(Entry::seq);
        serde_json::to_writer(&mut SizeLimit(2 * 1024 * 1024), &custom_entries)
            .map_err(|e| SessionError::invalid_entry(e.to_string()))?;
        deadline(query.deadline)?;
        let native_pending = crate::agent_harness::AgentHarness::saved_session_has_pending_work(
            &session,
            query.deadline,
        )
        .await
        .map_err(|e| SessionError::invalid_entry(e.to_string()))?;
        deadline(query.deadline)?;
        Ok(SavedSessionReadData {
            metadata: facts.metadata,
            values: facts.values,
            leaf_id,
            cursor_valid,
            messages,
            has_more,
            custom_entries,
            native_pending,
            progress_pending,
            idle_proof_supported,
        })
    }
}
