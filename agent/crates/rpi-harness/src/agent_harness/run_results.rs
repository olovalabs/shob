//! Stable run results reference committed entries, never the current lane tip.
use super::*;
use crate::session::types::RunResultIdentity;

impl AgentHarness {
    pub(super) async fn finish_run_result(
        &self,
        run_id: &str,
        outcome: &mut HarnessRunOutcome,
    ) -> HarnessResult<()> {
        let (operation, end, error, final_id, message) = match &*outcome {
            HarnessRunOutcome::Completed {
                final_entry_id,
                final_message,
                ..
            } => (
                OperationOutcome::Completed,
                RunEndOutcome::Completed,
                None,
                Some(final_entry_id.clone()),
                Some(final_message),
            ),
            HarnessRunOutcome::Aborted {
                final_entry_id,
                final_message,
                ..
            } => (
                OperationOutcome::Aborted,
                RunEndOutcome::Aborted,
                None,
                Some(final_entry_id.clone()),
                Some(final_message),
            ),
            HarnessRunOutcome::Failed {
                error,
                final_entry_id,
                final_message,
                ..
            } => (
                OperationOutcome::Failed,
                RunEndOutcome::Failed,
                Some(error.clone()),
                final_entry_id.clone(),
                final_message.as_ref(),
            ),
            HarnessRunOutcome::Suspended { .. } => return Ok(()),
        };
        // Usually the final entry is the assistant itself. If a result follows
        // it, walk only that committed suffix, not all session history.
        let mut assistant_id = None;
        if let Some(expected) = message {
            let mut id = final_id.clone();
            let mut seen = std::collections::HashSet::new();
            while let Some(current) = id {
                if !seen.insert(current.clone()) {
                    return Err(HarnessError::io("cycle in result branch"));
                }
                let entry = self
                    .session
                    .get_entry(&current)
                    .await
                    .map_err(session_to_harness_err)?
                    .ok_or_else(|| HarnessError::io("run result references a missing entry"))?;
                if matches!(&entry, Entry::Message(entry) if matches!(&entry.message, AgentMessage::Assistant(message) if message.as_ref() == expected))
                {
                    assistant_id = Some(current);
                    break;
                }
                id = entry.parent_id().map(str::to_owned);
            }
            if assistant_id.is_none() {
                return Err(HarnessError::io("terminal assistant was not committed"));
            }
        }
        let identity = RunResultIdentity {
            leaf_id: String::new(),
            final_entry_id: final_id,
            assistant_entry_id: assistant_id,
        };
        let leaf = self
            .finish_context_writes_with_result(
                run_id,
                Some(end),
                Some((operation, error, identity)),
            )
            .await?
            .unwrap_or_default();
        match outcome {
            HarnessRunOutcome::Completed { leaf_id, .. }
            | HarnessRunOutcome::Aborted { leaf_id, .. }
            | HarnessRunOutcome::Failed { leaf_id, .. } => *leaf_id = leaf,
            HarnessRunOutcome::Suspended { .. } => unreachable!(),
        }
        Ok(())
    }

    pub(super) async fn read_run_result(&self, run_id: &str) -> HarnessResult<Option<RunResult>> {
        let records = self
            .session
            .find_records(&RecordQuery {
                lane: Some(self.lane.clone()),
                run_id: Some(run_id.to_owned()),
                record_type: Some("operation_finished"),
                limit: Some(1),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)?;
        let Some(LaneRecord::OperationFinished(finished)) = records.into_iter().next() else {
            return Ok(None);
        };
        let started = self
            .session
            .find_records(&RecordQuery {
                lane: Some(self.lane.clone()),
                run_id: Some(run_id.to_owned()),
                record_type: Some("operation_started"),
                limit: Some(1),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)?;
        if !matches!(started.first(), Some(LaneRecord::OperationStarted(started)) if matches!(started.intent, OperationIntent::Run { .. }))
        {
            return Ok(None);
        }
        if finished.outcome == OperationOutcome::Declined {
            return Ok(None);
        }
        let identity = finished.result.ok_or_else(|| {
            HarnessError::io("legacy run has no reliable terminal result identity")
        })?;
        // Validate references even for Failed, without silently converting a
        // storage failure/missing entry into a fabricated success or None.
        for id in [
            Some(identity.leaf_id.as_str()).filter(|id| !id.is_empty()),
            identity.final_entry_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if self
                .session
                .get_entry(id)
                .await
                .map_err(session_to_harness_err)?
                .is_none()
            {
                return Err(HarnessError::io("run result references a missing entry"));
            }
        }
        let message = match identity.assistant_entry_id {
            Some(id) => match self
                .session
                .get_entry(&id)
                .await
                .map_err(session_to_harness_err)?
            {
                Some(Entry::Message(entry)) => match entry.message {
                    AgentMessage::Assistant(message) => Some(*message),
                    _ => {
                        return Err(HarnessError::io(
                            "run assistant reference is not an assistant",
                        ))
                    }
                },
                _ => return Err(HarnessError::io("run assistant entry is missing")),
            },
            None => None,
        };
        let outcome = match finished.outcome {
            OperationOutcome::Completed | OperationOutcome::Aborted => {
                let final_entry_id = identity
                    .final_entry_id
                    .ok_or_else(|| HarnessError::io("terminal run has no final entry"))?;
                let final_message =
                    message.ok_or_else(|| HarnessError::io("terminal run has no assistant"))?;
                if finished.outcome == OperationOutcome::Completed {
                    HarnessRunOutcome::Completed {
                        leaf_id: identity.leaf_id,
                        final_entry_id,
                        final_message,
                    }
                } else {
                    HarnessRunOutcome::Aborted {
                        leaf_id: identity.leaf_id,
                        final_entry_id,
                        final_message,
                    }
                }
            }
            OperationOutcome::Failed => HarnessRunOutcome::Failed {
                leaf_id: identity.leaf_id,
                final_entry_id: identity.final_entry_id,
                final_message: message,
                error: finished.error.unwrap_or_else(|| OperationError {
                    code: "failed".into(),
                    message: "run failed".into(),
                }),
            },
            OperationOutcome::Declined => return Ok(None),
        };
        Ok(Some(RunResult {
            run_id: run_id.to_owned(),
            outcome,
        }))
    }

    pub(super) async fn read_execution(
        &self,
        call_id: &str,
    ) -> HarnessResult<Option<ToolExecutionInfo>> {
        let entries = self
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    entry_type: Some("message"),
                    tool_call_id: Some(call_id.to_owned()),
                    order: Some(EntryOrder::NewestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .map_err(session_to_harness_err)?;
        let mut results = Vec::new();
        for entry in entries {
            let Entry::Message(message) = entry else {
                continue;
            };
            match message.message {
                AgentMessage::ToolResult(result) if result.tool_call_id == call_id => {
                    results.push((result.tool_name, message.base.id, message.base.seq));
                }
                AgentMessage::Assistant(assistant) => {
                    if let Some(call) = assistant.content.iter().find_map(|block| match block {
                        Content::ToolCall(call) if call.id == call_id => Some(call),
                        _ => None,
                    }) {
                        // A fork inherits its source lane's entries and native
                        // checkpoints. Match the selected branch assistant's exact
                        // identity below, not the destination lane or provider ID
                        // alone; results still have to be on this branch.
                        let started = self
                            .session
                            .find_records(&RecordQuery {
                                record_type: Some("tool_started"),
                                tool_call_id: Some(call_id.to_owned()),
                                order: Some(EntryOrder::NewestFirst),
                                ..Default::default()
                            })
                            .await
                            .map_err(session_to_harness_err)?;
                        let checkpoint = started.into_iter().find_map(|record| match record {
                            LaneRecord::ToolStarted(started)
                                if started.assistant_entry_id == message.base.id
                                    && started.tool_name == call.name =>
                            {
                                Some(started)
                            }
                            _ => None,
                        });
                        let result_entry_id = if let Some(checkpoint) = checkpoint {
                            let entries = self
                                .view()
                                .find_entries_on_branch(
                                    &EntryQuery {
                                        entry_id: Some(checkpoint.result_entry_id.clone()),
                                        limit: Some(1),
                                        ..Default::default()
                                    },
                                    &BranchBounds::default(),
                                )
                                .await
                                .map_err(session_to_harness_err)?;
                            match entries.first() {
                                None => None,
                                Some(Entry::Message(entry)) if matches!(&entry.message, AgentMessage::ToolResult(result) if result.tool_call_id == call.id && result.tool_name == call.name) => {
                                    Some(checkpoint.result_entry_id)
                                }
                                _ => {
                                    return Err(HarnessError::io(
                                        "tool checkpoint result identity does not match its result",
                                    ))
                                }
                            }
                        } else {
                            // Imported calls have no checkpoint. Bound their results
                            // to this assistant/user interval instead of pairing a
                            // reused provider id across a later conversation turn.
                            let boundary = self
                                .view()
                                .find_entries_on_branch(
                                    &EntryQuery {
                                        message_boundary_only: true,
                                        cursor: Some(crate::session::types::EntryCursor {
                                            after_seq: message.base.seq,
                                        }),
                                        order: Some(EntryOrder::OldestFirst),
                                        limit: Some(1),
                                        ..Default::default()
                                    },
                                    &BranchBounds::default(),
                                )
                                .await
                                .map_err(session_to_harness_err)?;
                            let end = boundary.first().map(Entry::seq).unwrap_or(u64::MAX);
                            results
                                .into_iter()
                                .find(|(name, _, seq)| name == &call.name && *seq < end)
                                .map(|(_, id, _)| id)
                        };
                        return Ok(Some(ToolExecutionInfo {
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            entry_id: Some(message.base.id),
                            result_entry_id,
                        }));
                    }
                    results.clear();
                }
                AgentMessage::User(_) => results.clear(),
                _ => {}
            }
        }
        Ok(None)
    }
}
