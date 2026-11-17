//! Branch-aware UI pages. Navigation may walk deep metadata chains, but retains
//! only a page and never reads lookahead/unselected canonical bodies.
use super::{
    engine::{self, Identity, Result},
    facts::types::*,
    index, queries,
};
use crate::error::SessionError;
use rusqlite::Connection;
use std::collections::VecDeque;

fn invalid(reason: &str) -> Box<dyn std::error::Error> {
    SessionError::invalid_query(reason).into()
}
fn limit(value: usize) -> Result<()> {
    if !(1..=100).contains(&value) {
        Err(invalid("limit must be between 1 and 100"))
    } else {
        Ok(())
    }
}
fn id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        Err(invalid("invalid entry cursor"))
    } else {
        Ok(())
    }
}
fn ancestor(db: &Connection, identity: &Identity, ancestor: u64, mut leaf: u64) -> Result<bool> {
    loop {
        if ancestor == leaf {
            return Ok(true);
        }
        if ancestor > leaf {
            return Ok(false);
        }
        match index::read_links(db, identity, leaf)?.parent {
            Some(parent) => leaf = parent,
            None => return Ok(false),
        }
    }
}
fn leaf(
    db: &Connection,
    identity: &Identity,
    lane: &str,
    pinned: Option<&str>,
) -> Result<Option<(String, u64)>> {
    let (state, _) = engine::restore(db, identity)?;
    let current = state
        .heads
        .get(lane)
        .ok_or_else(|| SessionError::invalid_lane("invalid lane"))?;
    let current = current
        .as_deref()
        .map(|id| -> Result<u64> {
            index::key_for_id(db, id)?.ok_or_else(|| "missing lane head".into())
        })
        .transpose()?;
    let selected = match pinned {
        Some(requested) => {
            id(requested)?;
            let pinned = index::key_for_id(db, requested)?
                .ok_or_else(|| invalid("branch_changed: snapshot leaf not found"))?;
            if !current
                .map(|current| ancestor(db, identity, pinned, current))
                .transpose()?
                .unwrap_or(false)
            {
                return Err(invalid(
                    "branch_changed: snapshot leaf is no longer on the selected branch",
                ));
            }
            Some(pinned)
        }
        None => current,
    };
    selected
        .map(|seq| {
            let meta = engine::read_metadata(db, identity, seq)?.0;
            let entry_id = meta.id().ok_or("invalid selected leaf")?.to_owned();
            index::read_links(db, identity, seq)?;
            Ok((entry_id, seq))
        })
        .transpose()
}
fn previous(db: &Connection, identity: &Identity, seq: u64, user: bool) -> Result<Option<u64>> {
    let meta = engine::read_metadata(db, identity, seq)?.0;
    engine::ensure(
        if user {
            meta.selection.user
        } else {
            meta.selection.transcript
        },
        "invalid navigation target type",
    )?;
    let links = index::read_links(db, identity, seq)?;
    Ok(if user {
        links.previous_user
    } else {
        links.previous_transcript
    })
}
pub(super) fn summaries(
    db: &Connection,
    identity: &Identity,
    lane: &str,
    q: &MessageSummaryQuery,
) -> Result<MessageSummaryPage> {
    limit(q.limit)?;
    if q.role != "user" {
        return Err(invalid("only role=user is supported"));
    }
    let leaf = leaf(db, identity, lane, q.leaf_id.as_deref())?;
    let mut next = leaf
        .as_ref()
        .map(|(_, seq)| index::read_links(db, identity, *seq).map(|l| l.nearest_user))
        .transpose()?
        .flatten();
    if let Some(before) = q.before_id.as_deref() {
        id(before)?;
        let cursor =
            index::key_for_id(db, before)?.ok_or_else(|| invalid("user cursor not found"))?;
        if !engine::read_metadata(db, identity, cursor)?
            .0
            .selection
            .user
        {
            return Err(invalid("cursor must identify a user message"));
        }
        while let Some(seq) = next {
            if seq == cursor {
                break;
            }
            next = previous(db, identity, seq, true)?;
        }
        if next.is_none() {
            return Err(invalid(
                "user cursor is not on the selected branch snapshot",
            ));
        }
        next = previous(db, identity, cursor, true)?;
    }
    let mut selected = Vec::with_capacity(q.limit);
    while let Some(seq) = next {
        if selected.len() == q.limit {
            break;
        }
        selected.push(seq);
        next = previous(db, identity, seq, true)?;
    }
    let has_more = next.is_some();
    selected.reverse();
    let messages = selected
        .into_iter()
        .map(|seq| index::read_summary(db, identity, seq))
        .collect::<Result<Vec<_>>>()?;
    let first_id = messages.first().map(|m| m.entry_id.clone());
    let last_id = messages.last().map(|m| m.entry_id.clone());
    Ok(MessageSummaryPage {
        filtered: true,
        role: "user".into(),
        messages,
        next_before_id: if has_more { first_id.clone() } else { None },
        first_id,
        last_id,
        has_more,
        leaf_id: leaf.map(|(id, _)| id),
    })
}
pub(super) fn window(
    db: &Connection,
    identity: &Identity,
    lane: &str,
    q: &MessageWindowQuery,
) -> Result<MessageWindowPage> {
    limit(q.limit)?;
    let cursors = [
        q.anchor_id.as_deref(),
        q.before_id.as_deref(),
        q.after_id.as_deref(),
    ];
    if cursors.iter().filter(|v| v.is_some()).count() != 1 {
        return Err(invalid(
            "exactly one of anchorId, beforeId or afterId is required",
        ));
    }
    let cursor = cursors
        .into_iter()
        .flatten()
        .next()
        .ok_or("missing cursor")?;
    id(cursor)?;
    let leaf = leaf(db, identity, lane, q.leaf_id.as_deref())?;
    let anchor = index::key_for_id(db, cursor)?
        .ok_or_else(|| invalid("message cursor not on selected branch snapshot"))?;
    let cursor_meta = engine::read_metadata(db, identity, anchor)?.0;
    if !cursor_meta.selection.transcript {
        if leaf
            .as_ref()
            .map(|(_, seq)| ancestor(db, identity, anchor, *seq))
            .transpose()?
            .unwrap_or(false)
        {
            return Err(invalid("cursor must identify a transcript message"));
        }
        return Err(invalid("message cursor not on selected branch snapshot"));
    }
    let mut next = leaf
        .as_ref()
        .map(|(_, seq)| index::read_links(db, identity, *seq).map(|l| l.nearest_transcript))
        .transpose()?
        .flatten();
    let mut newer = VecDeque::with_capacity(q.limit + 1);
    let mut discarded_newer = false;
    while let Some(seq) = next {
        if seq == anchor {
            break;
        }
        if seq < anchor {
            next = None;
            break;
        }
        newer.push_back(seq);
        if newer.len() > q.limit + 1 {
            newer.pop_front();
            discarded_newer = true;
        }
        next = previous(db, identity, seq, false)?;
    }
    if next.is_none() {
        return Err(invalid("message cursor not on selected branch snapshot"));
    }
    let newer = newer.into_iter().rev().collect::<Vec<_>>();
    next = previous(db, identity, anchor, false)?;
    let mut older = Vec::with_capacity(q.limit + 1);
    while let Some(seq) = next {
        older.push(seq);
        if older.len() > q.limit {
            break;
        }
        next = previous(db, identity, seq, false)?;
    }
    let (selected, mut has_more_before, mut has_more_after) = if q.before_id.is_some() {
        (
            older
                .iter()
                .take(q.limit)
                .rev()
                .copied()
                .collect::<Vec<_>>(),
            older.len() > q.limit,
            true,
        )
    } else if q.after_id.is_some() {
        (
            newer.iter().take(q.limit).copied().collect::<Vec<_>>(),
            true,
            newer.len() > q.limit || discarded_newer,
        )
    } else {
        let before = older.len().min(q.limit / 3);
        let after = newer.len().min(q.limit - 1 - before);
        let before = older.len().min(q.limit - 1 - after);
        let mut selected = older.iter().take(before).rev().copied().collect::<Vec<_>>();
        selected.push(anchor);
        selected.extend(newer.iter().take(after).copied());
        (
            selected,
            older.len() > before,
            newer.len() > after || discarded_newer,
        )
    };
    let entries = selected
        .into_iter()
        .map(|seq| queries::entry(db, identity, seq))
        .collect::<Result<Vec<_>>>()?;
    if entries.is_empty() {
        has_more_before = false;
        has_more_after = false;
    }
    let first_id = entries.first().map(|e| e.id().into());
    let last_id = entries.last().map(|e| e.id().into());
    Ok(MessageWindowPage {
        entries,
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
        first_id,
        last_id,
        has_more_before,
        has_more_after,
        leaf_id: leaf.map(|(id, _)| id),
    })
}
