//! Conditional extension writes and a branch-local, durable notice outbox.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BranchWriteGuard {
    pub session_id: String,
    pub leaf_id: Option<String>,
    pub custom_type: String,
    pub entry_id: Option<String>,
}

pub enum ScopedEntry {
    Custom {
        custom_type: String,
        data: Option<JsonValue>,
    },
    Notice {
        message: AgentMessage,
        key: String,
    },
}

const OUTBOX: &str = "__rpiOutbox";

fn notice_metadata(message: &AgentMessage) -> Option<&JsonValue> {
    match message {
        AgentMessage::Custom(custom) => custom.data.get(OUTBOX),
        _ => None,
    }
}

fn matching_notice(
    existing: &AgentMessage,
    requested: &AgentMessage,
    key: &str,
) -> HarnessResult<bool> {
    if notice_metadata(existing)
        .and_then(|value| value.get("key"))
        .and_then(JsonValue::as_str)
        != Some(key)
    {
        return Ok(false);
    }
    let mut existing = existing.clone();
    if let AgentMessage::Custom(custom) = &mut existing {
        custom.data.as_object_mut().unwrap().remove(OUTBOX);
    }
    if &existing != requested {
        return Err(HarnessError::io("outbox key reused for a different notice"));
    }
    Ok(true)
}

impl AgentHarness {
    async fn guarded_branch(&self, guard: &BranchWriteGuard) -> HarnessResult<()> {
        {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            if inner.active_run.as_ref().is_some_and(|run| run.recovering) {
                return Err(HarnessError::io(
                    "branch write conflict: recovery in progress",
                ));
            }
        }
        if guard.session_id.is_empty()
            || guard.custom_type.trim().is_empty()
            || self.session.storage().metadata().id != guard.session_id
            || self
                .view()
                .get_leaf_id()
                .await
                .map_err(session_to_harness_err)?
                != guard.leaf_id
        {
            return Err(HarnessError::io(
                "branch write conflict: session or leaf changed",
            ));
        }
        let entries = self
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    entry_type: Some("custom"),
                    custom_type: Some(guard.custom_type.clone()),
                    order: Some(EntryOrder::NewestFirst),
                    limit: Some(1),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .map_err(session_to_harness_err)?;
        let latest = entries.first().map(Entry::id);
        if latest != guard.entry_id.as_deref() {
            return Err(HarnessError::io(
                "branch write conflict: extension state changed",
            ));
        }
        Ok(())
    }

    /// Validate and accept under the same admission gate used by navigation and
    /// session switching. No plugin callback is invoked while the gate is held.
    pub async fn append_scoped_entry(
        &self,
        guard: &BranchWriteGuard,
        entry: ScopedEntry,
    ) -> HarnessResult<String> {
        // A valid extension branch guard is not native journal authority. This
        // public route uses conditional append, so enforce the same reservation
        // as ordinary Session appends before any branch payload lookup.
        if matches!(&entry, ScopedEntry::Custom { custom_type, .. }
            if crate::tool_program_journal::is_reserved_custom_type(custom_type))
        {
            return Err(HarnessError::io("reserved native child journal type"));
        }
        let gate = self.message_writes.lock().await;
        // Pin the revision before every preflight read, including outbox
        // deduplication. The backend compares it inside its write boundary.
        let revision = self
            .session
            .storage()
            .history_revision()
            .await
            .map_err(session_to_harness_err)?;
        self.guarded_branch(guard).await?;
        match entry {
            ScopedEntry::Custom { custom_type, data } => {
                if custom_type != guard.custom_type {
                    return Err(HarnessError::io("guard custom type mismatch"));
                }
                self.session
                    .append_entry_if_revision(
                        ProvisionedEntry {
                            id: self.next_entry_id(),
                            kind: ProvisionedKind::Custom { custom_type, data },
                        },
                        &self.lane,
                        revision,
                    )
                    .await
                    .map(|entry| entry.id().to_owned())
                    .map_err(session_to_harness_err)
            }
            ScopedEntry::Notice { mut message, key } => {
                if key.is_empty() || key.len() > 256 {
                    return Err(HarnessError::io("invalid outbox key"));
                }
                let source = guard
                    .entry_id
                    .as_ref()
                    .ok_or_else(|| HarnessError::io("notice requires a source state entry"))?;
                match &message {
                    AgentMessage::Custom(custom)
                        if custom.data.is_object() && custom.data.get(OUTBOX).is_none() => {}
                    _ => {
                        return Err(HarnessError::io(
                            "outbox requires a custom message without reserved metadata",
                        ))
                    }
                }
                let entries = self
                    .view()
                    .find_entries_on_branch(
                        &EntryQuery {
                            // Outbox notices are custom messages, never archival
                            // custom entries. Select metadata before hydration.
                            entry_type: Some("message"),
                            outbox_key: Some(key.clone()),
                            limit: Some(1),
                            ..Default::default()
                        },
                        &BranchBounds {
                            stop_at_type: Some("compaction"),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(session_to_harness_err)?;
                for entry in &entries {
                    if let Entry::Message(entry) = entry {
                        if matching_notice(&entry.message, &message, &key)? {
                            self.check_notice_revision(revision).await?;
                            // This branch entry is already consumed. An ACK
                            // must not manufacture pending work or redeliver it.
                            return Ok(entry.base.id.clone());
                        }
                    }
                }
                // Pending context entries exclude consumed targets and notices
                // sourced from another branch, but survive a process restart.
                for entry in self.pending_context_entries().await? {
                    if let ProvisionedKind::Message {
                        message: existing, ..
                    } = entry.kind
                    {
                        if matching_notice(&existing, &message, &key)? {
                            self.check_notice_revision(revision).await?;
                            // The storage owner may have committed after the
                            // original caller was dropped, before it published
                            // this hint. Only an accepted, still-pending target
                            // can restore scheduling; no durable write is needed.
                            self.inner
                                .lock()
                                .unwrap()
                                .pending_context_lanes
                                .insert(self.lane.clone());
                            return Ok(entry.id);
                        }
                    }
                }
                if let AgentMessage::Custom(custom) = &mut message {
                    custom.data[OUTBOX] = serde_json::json!({"key":key, "sourceEntryId":source});
                }
                // Always accept through the durable outbox first, even while
                // idle. This makes deduplication and queue acceptance one
                // conditional write; pending-context replay keeps the same ID.
                let id = self.next_entry_id();
                let target = Self::provisioned_target(&ProvisionedEntry {
                    id: id.clone(),
                    kind: ProvisionedKind::Message {
                        message,
                        terminate: None,
                    },
                });
                self.session
                    .append_record_if_revision(
                        LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
                            base: self.record_base(),
                            queue: QueueKind::Context,
                            run_id: None,
                            target,
                        }),
                        revision,
                    )
                    .await
                    .map_err(session_to_harness_err)?;
                self.inner
                    .lock()
                    .unwrap()
                    .pending_context_lanes
                    .insert(self.lane.clone());
                let active = self
                    .inner
                    .lock()
                    .unwrap()
                    .active_run
                    .as_ref()
                    .is_some_and(|run| run.lane == self.lane);
                let open = self
                    .session
                    .find_open_operations(&self.lane, Some(1))
                    .await
                    .map_err(session_to_harness_err)?;
                let messages = if !active && open.is_empty() {
                    self.flush_context_messages_locked().await?
                } else {
                    Vec::new()
                };
                drop(gate);
                self.emit_appended_messages(messages).await;
                Ok(id)
            }
        }
    }

    async fn check_notice_revision(&self, expected: u64) -> HarnessResult<()> {
        let current = self
            .session
            .storage()
            .history_revision()
            .await
            .map_err(session_to_harness_err)?;
        if current != expected {
            return Err(HarnessError::io(
                "branch write conflict: history changed during notice lookup",
            ));
        }
        Ok(())
    }

    /// Outbox context stays with its source branch. Other queued context keeps
    /// the pre-existing lane-wide semantics. A skipped notice can be drained
    /// when navigating back to its source; it is never cancelled pre-commit.
    pub(super) async fn notice_source_on_branch(
        &self,
        message: &AgentMessage,
    ) -> HarnessResult<bool> {
        let Some(metadata) = notice_metadata(message) else {
            return Ok(true);
        };
        let source = metadata
            .get("sourceEntryId")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| HarnessError::io("invalid outbox source entry"))?;
        let entries = self
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    entry_id: Some(source.to_owned()),
                    limit: Some(1),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .map_err(session_to_harness_err)?;
        Ok(!entries.is_empty())
    }
}

#[cfg(test)]
mod tests;
