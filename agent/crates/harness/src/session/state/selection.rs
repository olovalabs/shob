//! Shared branch walks and query candidate selection. No payload is inspected.
use super::*;

impl<P: Clone> IndexedState<P> {
    pub(crate) fn entry_index(&self, id: &str) -> Option<usize> {
        self.entries_by_id.get(id).copied()
    }
    pub(crate) fn metadata(&self) -> &[EntryMeta] {
        &self.metadata
    }
    pub(crate) fn entry_meta(&self, index: usize) -> Option<&EntryMeta> {
        self.metadata.get(index)
    }
    pub(crate) fn payload(&self, index: usize) -> Option<&P> {
        self.entries.get(index)
    }
    /// Replace a cold reference with a hydrated payload without touching facts.
    pub(crate) fn payload_mut(&mut self, index: usize) -> Option<&mut P> {
        self.entries.get_mut(index)
    }
    pub(crate) fn records(&self) -> &[LaneRecord] {
        &self.records
    }

    /// Newest to oldest, inclusive of the first bound, preserving eager errors.
    pub(crate) fn walk_to_root(
        &self,
        start: Option<&str>,
        bounds: &BranchBounds,
    ) -> SessionResult<Vec<usize>> {
        let mut path = Vec::new();
        let mut next_id = start;
        let mut visited = std::collections::HashSet::new();
        while let Some(id) = next_id.take() {
            let index = self
                .entry_index(id)
                .ok_or_else(|| SessionError::not_found(format!("Entry not found: {id}")))?;
            let current = &self.metadata[index];
            if !visited.insert(index) {
                return Err(SessionError::invalid_entry(format!(
                    "Session branch contains a cycle at {}",
                    current.id()
                )));
            }
            let reached = bounds.stop_at_id.as_deref() == Some(current.id())
                || bounds.stop_at_type.is_some_and(|kind| kind == current.kind)
                || current.parent_id().is_none();
            path.push(index);
            if reached {
                break;
            }
            next_id = current.parent_id();
        }
        Ok(path)
    }

    pub(crate) fn branch_ancestry(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<crate::session::types::EntryAncestry>> {
        Ok(self
            .walk_to_root(Some(start), bounds)?
            .into_iter()
            .map(|index| {
                let meta = &self.metadata[index];
                crate::session::types::EntryAncestry {
                    id: meta.id.clone(),
                    parent_id: meta.parent_id.clone(),
                }
            })
            .collect())
    }

    /// Ordered metadata candidates. Deliberately does NOT enforce query.limit:
    /// callers must hydrate and run matches_entry_query before counting matches.
    /// This is essential for tool_call_id and outbox_key predicates.
    pub(crate) fn select_entry_indices(
        &self,
        query: &EntryQuery,
        branch: Option<(&BranchBounds, &str)>,
    ) -> SessionResult<Vec<usize>> {
        validate_limit(query.limit)?;
        let order = query.order.unwrap_or_default();
        let mut indices = if let Some((bounds, start)) = branch {
            let mut path = self.walk_to_root(Some(start), bounds)?;
            if order == EntryOrder::OldestFirst {
                path.reverse();
                // Preserve the existing oldest-first inclusive-bound semantics.
                if let Some(end) = path.iter().position(|&index| {
                    let meta = &self.metadata[index];
                    bounds.stop_at_id.as_deref() == Some(meta.id())
                        || bounds.stop_at_type.is_some_and(|kind| kind == meta.kind)
                }) {
                    path.truncate(end + 1);
                }
            }
            path
        } else {
            let mut indices: Vec<_> = (0..self.metadata.len()).collect();
            if order == EntryOrder::NewestFirst {
                indices.reverse();
            }
            indices
        };
        indices.retain(|&index| self.metadata[index].matches_query(query));
        Ok(indices)
    }

    pub(crate) fn log_items(&self, options: &LogOptions) -> SessionResult<Vec<&StoredLogItem>> {
        validate_limit(options.limit)?;
        Ok(self
            .log
            .iter()
            .filter(|item| {
                let seq = match item {
                    StoredLogItem::Entry(index) => self.metadata[*index].seq,
                    StoredLogItem::Record(index) => self.records[*index].seq(),
                    StoredLogItem::Other(item) => item.seq(),
                };
                options.after_seq.is_none_or(|after| seq > after)
            })
            .take(options.limit.unwrap_or(usize::MAX))
            .collect())
    }
}
