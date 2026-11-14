//! Bounded compatibility reads shared by storage and tree implementations.
//! A read pins its source and leaf once; immutable parent chains then provide
//! a logical snapshot even if a lane advances while the read is in flight.
use std::future::Future;
use std::io::{self, Write};

use super::state::EntryMeta;
use super::types::*;
use crate::error::{SessionError, SessionResult};

fn validate_id(id: &str) -> SessionResult<()> {
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err(SessionError::invalid_query("invalid entry cursor"));
    }
    Ok(())
}
fn validate_page(query: &BranchEntryPageQuery) -> SessionResult<()> {
    if !(1..=100).contains(&query.limit) {
        return Err(SessionError::invalid_query(
            "limit must be between 1 and 100",
        ));
    }
    if !(1..=16 * 1024 * 1024).contains(&query.max_bytes) {
        return Err(SessionError::invalid_query(
            "max_bytes must be between 1 and 16777216",
        ));
    }
    if query.before_id.is_some() && query.after_id.is_some() {
        return Err(SessionError::invalid_query(
            "before_id and after_id are mutually exclusive",
        ));
    }
    for id in [&query.leaf_id, &query.before_id, &query.after_id]
        .into_iter()
        .flatten()
    {
        validate_id(id)?;
    }
    Ok(())
}
async fn lane_leaf<S: SessionStorage + ?Sized>(
    storage: &S,
    lane: &str,
) -> SessionResult<Option<String>> {
    storage
        .get_lanes()
        .await?
        .into_iter()
        .find(|pointer| pointer.lane == lane)
        .map(|pointer| pointer.leaf_id)
        .ok_or_else(|| SessionError::invalid_lane("invalid lane"))
}

pub(crate) async fn storage_branch_entry_page<S: SessionStorage + ?Sized>(
    storage: &S,
    lane: &str,
    query: &BranchEntryPageQuery,
) -> SessionResult<BranchEntryPage> {
    validate_page(query)?;
    let leaf = lane_leaf(storage, lane).await?;
    page(leaf, query, |query, start| async move {
        storage
            .find_entries_on_branch(&query, &BranchBounds::default(), &start)
            .await
    })
    .await
}
pub(crate) async fn tree_branch_entry_page<T: SessionTree + ?Sized>(
    tree: &T,
    query: &BranchEntryPageQuery,
) -> SessionResult<BranchEntryPage> {
    validate_page(query)?;
    let leaf = tree.get_leaf_id().await?;
    page(leaf, query, |query, start| async move {
        tree.find_entries_on_branch(
            &query,
            &BranchBounds {
                start: Some(start),
                ..Default::default()
            },
        )
        .await
    })
    .await
}
pub(crate) async fn storage_branch_overview<S: SessionStorage + ?Sized>(
    storage: &S,
    lane: &str,
) -> SessionResult<BranchOverview> {
    let leaf = lane_leaf(storage, lane).await?;
    overview(leaf, |query, start| async move {
        storage
            .find_entries_on_branch(&query, &BranchBounds::default(), &start)
            .await
    })
    .await
}
pub(crate) async fn tree_branch_overview<T: SessionTree + ?Sized>(
    tree: &T,
) -> SessionResult<BranchOverview> {
    let leaf = tree.get_leaf_id().await?;
    overview(leaf, |query, start| async move {
        tree.find_entries_on_branch(
            &query,
            &BranchBounds {
                start: Some(start),
                ..Default::default()
            },
        )
        .await
    })
    .await
}

fn scan(order: EntryOrder, cursor: Option<u64>, limit: usize) -> EntryQuery {
    EntryQuery {
        order: Some(order),
        cursor: cursor.map(|after_seq| EntryCursor { after_seq }),
        limit: Some(limit),
        ..Default::default()
    }
}
async fn member<F, Fut>(read: &mut F, leaf: &str, id: &str) -> SessionResult<Option<Entry>>
where
    F: FnMut(EntryQuery, String) -> Fut,
    Fut: Future<Output = SessionResult<Vec<Entry>>>,
{
    Ok(read(
        EntryQuery {
            entry_id: Some(id.into()),
            limit: Some(1),
            ..Default::default()
        },
        leaf.into(),
    )
    .await?
    .pop())
}
async fn exists<F, Fut>(
    read: &mut F,
    leaf: &str,
    order: EntryOrder,
    seq: u64,
) -> SessionResult<bool>
where
    F: FnMut(EntryQuery, String) -> Fut,
    Fut: Future<Output = SessionResult<Vec<Entry>>>,
{
    Ok(!read(scan(order, Some(seq), 1), leaf.into())
        .await?
        .is_empty())
}
async fn page<F, Fut>(
    current: Option<String>,
    query: &BranchEntryPageQuery,
    mut read: F,
) -> SessionResult<BranchEntryPage>
where
    F: FnMut(EntryQuery, String) -> Fut,
    Fut: Future<Output = SessionResult<Vec<Entry>>>,
{
    let leaf = match &query.leaf_id {
        Some(pinned) => {
            let on_branch = match &current {
                Some(current) if current == pinned => true,
                Some(current) => member(&mut read, current, pinned).await?.is_some(),
                None => false,
            };
            if !on_branch {
                return Err(SessionError::invalid_query(
                    "branch_changed: snapshot leaf is no longer on the selected branch",
                ));
            }
            Some(pinned.clone())
        }
        None => current,
    };
    let cursor_id = query.before_id.as_ref().or(query.after_id.as_ref());
    let cursor = if let Some(id) = cursor_id {
        let found = match &leaf {
            Some(leaf) => member(&mut read, leaf, id).await?,
            None => None,
        };
        Some(
            found
                .ok_or_else(|| {
                    SessionError::invalid_query("cursor must belong to the selected branch")
                })?
                .seq(),
        )
    } else {
        None
    };
    let order = if query.after_id.is_some() {
        EntryOrder::OldestFirst
    } else {
        EntryOrder::NewestFirst
    };
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    if let Some(leaf) = &leaf {
        let mut next = cursor;
        while entries.len() < query.limit {
            let Some(entry) = read(scan(order, next, 1), leaf.clone()).await?.pop() else {
                break;
            };
            let size = encoded_entry_bytes(&entry)?;
            if !entries.is_empty() && bytes.saturating_add(size) > query.max_bytes {
                break;
            }
            next = Some(entry.seq());
            bytes = bytes.saturating_add(size);
            entries.push(entry);
            if bytes >= query.max_bytes {
                break;
            }
        }
    }
    if order == EntryOrder::NewestFirst {
        entries.reverse();
    }
    let first_id = entries.first().map(|entry| entry.id().to_owned());
    let last_id = entries.last().map(|entry| entry.id().to_owned());
    let (has_more_before, has_more_after) = if let Some(leaf) = &leaf {
        if let (Some(first), Some(last)) = (entries.first(), entries.last()) {
            (
                exists(&mut read, leaf, EntryOrder::NewestFirst, first.seq()).await?,
                exists(&mut read, leaf, EntryOrder::OldestFirst, last.seq()).await?,
            )
        } else {
            // An empty exclusive edge still has history on the cursor's side.
            (query.after_id.is_some(), query.before_id.is_some())
        }
    } else {
        (false, false)
    };
    Ok(BranchEntryPage {
        next_before_id: if has_more_before {
            first_id.clone()
        } else {
            None
        },
        next_after_id: if has_more_after {
            last_id.clone()
        } else {
            None
        },
        entries,
        first_id,
        last_id,
        has_more_before,
        has_more_after,
        leaf_id: leaf,
    })
}

struct ByteCount(usize);
impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// Serialize the concrete arm: Entry's legacy serializer builds a JSON Value
/// (copying image/text payloads), which a bounded read must not do just to count.
fn encoded_entry_bytes(entry: &Entry) -> SessionResult<usize> {
    let mut count = ByteCount(0);
    let result = match entry {
        Entry::Message(value) => serde_json::to_writer(&mut count, value),
        Entry::ModelChange(value) => serde_json::to_writer(&mut count, value),
        Entry::ThinkingLevel(value) => serde_json::to_writer(&mut count, value),
        Entry::ActiveTools(value) => serde_json::to_writer(&mut count, value),
        Entry::Compaction(value) => serde_json::to_writer(&mut count, value),
        Entry::BranchSummary(value) => serde_json::to_writer(&mut count, value),
        Entry::Custom(value) => serde_json::to_writer(&mut count, value),
    };
    result.map_err(|error| {
        SessionError::invalid_payload(format!("cannot encode branch entry: {error}"))
    })?;
    Ok(count.0)
}

/// Consume metadata chronologically. Shared with cold-payload storage backends;
/// compaction/summary usages are deliberately excluded from assistant totals.
pub(crate) fn overview_from_metadata<'a>(
    entries: impl IntoIterator<Item = &'a EntryMeta>,
) -> BranchOverview {
    let mut result = BranchOverview {
        thinking_level: "off".into(),
        model: None,
        active_tool_names: None,
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cost_total: 0.0,
        cache_hit_rate: None,
        context_tokens: None,
    };
    for entry in entries {
        update_overview(&mut result, entry);
    }
    result
}
fn update_overview(result: &mut BranchOverview, entry: &EntryMeta) {
    if let Some(level) = &entry.thinking_level {
        result.thinking_level.clone_from(level);
    }
    if let Some(model) = &entry.model_change {
        result.model = Some(model.clone());
    }
    if let (Some(provider), Some(model)) = (&entry.assistant_provider, &entry.assistant_model) {
        result.model = Some((provider.clone(), model.clone()));
    }
    if let Some(tools) = &entry.active_tools {
        result.active_tool_names = Some(tools.clone());
    }
    if entry.kind == "compaction" {
        result.context_tokens = None;
    }
    if entry.kind != "message" || entry.assistant_model.is_none() {
        return;
    }
    let Some(usage) = &entry.usage else { return };
    result.input += usage.input;
    result.output += usage.output;
    result.cache_read += usage.cache_read;
    result.cache_write += usage.cache_write;
    result.cost_total += usage.cost.total;
    let denominator = usage.input + usage.cache_read + usage.cache_write;
    if denominator > 0 && usage.cache_read + usage.cache_write > 0 {
        result.cache_hit_rate = Some(usage.cache_read as f64 / denominator as f64 * 100.0);
    }
    if !matches!(
        entry.stop_reason,
        Some(rpi_ai::types::StopReason::Aborted | rpi_ai::types::StopReason::Error)
    ) {
        let tokens = usage.context_tokens();
        if tokens > 0 {
            result.context_tokens = Some(tokens);
        }
    }
}
async fn overview<F, Fut>(leaf: Option<String>, mut read: F) -> SessionResult<BranchOverview>
where
    F: FnMut(EntryQuery, String) -> Fut,
    Fut: Future<Output = SessionResult<Vec<Entry>>>,
{
    let mut result = overview_from_metadata(std::iter::empty());
    let Some(leaf) = leaf else { return Ok(result) };
    let mut cursor = None;
    loop {
        let batch = read(scan(EntryOrder::OldestFirst, cursor, 16), leaf.clone()).await?;
        if batch.is_empty() {
            break;
        }
        for entry in &batch {
            update_overview(&mut result, &EntryMeta::from_entry(entry));
        }
        cursor = batch.last().map(Entry::seq);
    }
    Ok(result)
}
