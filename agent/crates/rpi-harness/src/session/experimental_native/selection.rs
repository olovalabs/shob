//! Payload-free query keys, validated together with the fact metadata digest.
use super::facts::types::*;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbox: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub boundary: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub transcript: bool,
}
impl Selection {
    pub fn of(mutation: &SessionMutation) -> Self {
        let mut s = Self::default();
        match mutation {
            SessionMutation::Entry { entry, .. } => {
                s.entry_type = Some(entry.entry_type().into());
                s.transcript = matches!(entry, Entry::Message(_) | Entry::Compaction(_));
                if let Entry::Custom(e) = entry {
                    s.custom_type = Some(e.custom_type.clone());
                }
                if let Entry::Message(e) = entry {
                    use rpi_agent::AgentMessage;
                    match &e.message {
                        AgentMessage::Assistant(a) => {
                            s.boundary = true;
                            s.tools = a
                                .content
                                .iter()
                                .filter_map(|c| {
                                    if let rpi_ai::types::Content::ToolCall(t) = c {
                                        Some(t.id.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                        }
                        AgentMessage::User(_) => {
                            s.boundary = true;
                            s.user = true;
                        }
                        AgentMessage::ToolResult(t) => s.tools.push(t.tool_call_id.clone()),
                        AgentMessage::Custom(c) => {
                            s.outbox = c
                                .data
                                .get("__rpiOutbox")
                                .and_then(|v| v.get("key"))
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        }
                    }
                }
            }
            SessionMutation::Record { record } => {
                s.record_type = Some(record.record_type().into());
                s.run = match record {
                    LaneRecord::OperationStarted(r) => Some(r.base.id.clone()),
                    _ => record.run_id().map(str::to_owned),
                };
                match record {
                    LaneRecord::OperationStarted(r) => s.operation = Some(r.intent.kind().into()),
                    LaneRecord::QueueEnqueued(r) => {
                        s.queue = Some(r.queue);
                        s.target = r
                            .target
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                    }
                    LaneRecord::ToolStarted(r) => s.tools.push(r.tool_call_id.clone()),
                    _ => {}
                }
            }
            _ => {}
        }
        s
    }
    pub fn entry_matches(&self, id: &str, seq: u64, q: &EntryQuery) -> bool {
        q.entry_type
            .is_none_or(|t| self.entry_type.as_deref() == Some(t))
            && q.custom_type
                .as_ref()
                .is_none_or(|t| self.custom_type.as_ref() == Some(t))
            // This metadata oracle is also compiled by the standalone recovery
            // fixture; keep exact wire keys independent of its crate root.
            && (!q.exclude_tool_program_bodies
                || !matches!(
                    self.custom_type.as_deref(),
                    Some("rpi.tool_program.body_chunk.v1" | "rpi.tool_program.body_commit.v1")
                ))
            && q.entry_id.as_deref().is_none_or(|t| t == id)
            && q.tool_call_id
                .as_ref()
                .is_none_or(|t| self.tools.contains(t))
            && q.outbox_key
                .as_ref()
                .is_none_or(|t| self.outbox.as_ref() == Some(t))
            && (!q.message_boundary_only || self.boundary)
            && q.cursor.is_none_or(|c| {
                if q.order == Some(EntryOrder::OldestFirst) {
                    seq > c.after_seq
                } else {
                    seq < c.after_seq
                }
            })
    }
    pub fn record_matches(&self, lane: &str, seq: u64, q: &RecordQuery) -> bool {
        q.lane.as_deref().is_none_or(|l| l == lane)
            && q.record_type
                .is_none_or(|t| self.record_type.as_deref() == Some(t))
            && q.run_id
                .as_ref()
                .is_none_or(|r| self.run.as_ref() == Some(r))
            && q.operation_kind
                .is_none_or(|k| self.operation.as_deref() == Some(k))
            && q.queue_kind.is_none_or(|k| self.queue == Some(k))
            && q.target_entry_id
                .as_ref()
                .is_none_or(|t| self.target.as_ref() == Some(t))
            && q.tool_call_id.as_ref().is_none_or(|t| {
                self.record_type.as_deref() == Some("tool_started") && self.tools.contains(t)
            })
            && q.after_seq.is_none_or(|a| seq > a)
    }
}
