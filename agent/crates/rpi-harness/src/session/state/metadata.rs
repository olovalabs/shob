//! Small, typed facts derived from a validated entry; never a placeholder Entry.
use crate::session::types::{is_tool_program_body_type, Entry, EntryOrder, EntryQuery};
use rpi_agent::message::{AgentMessage, AgentMessageRole};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EntryMeta {
    pub(crate) kind: &'static str,
    pub(crate) id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) seq: u64,
    pub(crate) timestamp: i64,
    pub(crate) role: Option<AgentMessageRole>,
    pub(crate) custom_type: Option<String>,
    pub(crate) message_timestamp: Option<i64>,
    pub(crate) assistant_provider: Option<String>,
    pub(crate) assistant_model: Option<String>,
    pub(crate) model_change: Option<(String, String)>,
    pub(crate) thinking_level: Option<String>,
    pub(crate) active_tools: Option<Vec<String>>,
    pub(crate) usage: Option<rpi_ai::types::Usage>,
    /// Number of messages in the default, hook-free context projection.
    /// Retained compaction tails deliberately include Deferred messages.
    pub(crate) context_message_count: usize,
    pub(crate) stop_reason: Option<rpi_ai::types::StopReason>,
}

impl EntryMeta {
    pub(crate) fn from_entry(entry: &Entry) -> Self {
        let mut meta = Self {
            kind: entry.entry_type(),
            id: entry.id().into(),
            parent_id: entry.parent_id().map(str::to_owned),
            seq: entry.seq(),
            timestamp: entry.base().timestamp,
            role: entry.message_role(),
            custom_type: None,
            message_timestamp: None,
            assistant_provider: None,
            assistant_model: None,
            model_change: None,
            thinking_level: None,
            active_tools: None,
            usage: None,
            context_message_count: match entry {
                Entry::Message(message) => usize::from(!matches!(
                    &message.message,
                    AgentMessage::Assistant(assistant)
                        if assistant.stop_reason == rpi_ai::types::StopReason::Deferred
                )),
                Entry::Compaction(compaction) => 1 + compaction.retained_tail.len(),
                Entry::BranchSummary(summary) => usize::from(!summary.summary.is_empty()),
                _ => 0,
            },
            stop_reason: None,
        };
        match entry {
            Entry::Message(message) => match &message.message {
                AgentMessage::Assistant(assistant) => {
                    meta.message_timestamp = Some(assistant.timestamp);
                    meta.assistant_provider = Some(assistant.provider.clone());
                    meta.assistant_model = Some(assistant.model.clone());
                    meta.usage = Some(assistant.usage.clone());
                    meta.stop_reason = Some(assistant.stop_reason.clone());
                }
                AgentMessage::User(user) => meta.message_timestamp = Some(user.timestamp),
                _ => {}
            },
            Entry::ModelChange(change) => {
                meta.model_change = Some((change.provider.clone(), change.model_id.clone()))
            }
            Entry::ThinkingLevel(change) => {
                meta.thinking_level = Some(change.thinking_level.clone())
            }
            Entry::ActiveTools(change) => {
                meta.active_tools = Some(change.active_tool_names.clone())
            }
            Entry::Custom(custom) => meta.custom_type = Some(custom.custom_type.clone()),
            Entry::Compaction(compaction) => meta.usage = compaction.usage.clone(),
            Entry::BranchSummary(summary) => meta.usage = summary.usage.clone(),
        }
        meta
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }
    pub(crate) fn is_user(&self) -> bool {
        matches!(self.role, Some(AgentMessageRole::User))
    }
    pub(crate) fn is_transcript(&self) -> bool {
        matches!(self.kind, "message" | "compaction")
    }

    /// Only reject using facts actually in the index. Tool/outbox predicates
    /// MUST be applied to the complete hydrated entry, before enforcing limit.
    pub(crate) fn matches_query(&self, query: &EntryQuery) -> bool {
        !(query.exclude_tool_program_bodies
            && self
                .custom_type
                .as_deref()
                .is_some_and(is_tool_program_body_type))
            && query.entry_type.is_none_or(|kind| self.kind == kind)
            && query
                .custom_type
                .as_deref()
                .is_none_or(|kind| self.custom_type.as_deref() == Some(kind))
            && query.entry_id.as_deref().is_none_or(|id| self.id == id)
            && query
                .cursor
                .is_none_or(|cursor| match query.order.unwrap_or_default() {
                    EntryOrder::OldestFirst => self.seq > cursor.after_seq,
                    EntryOrder::NewestFirst => self.seq < cursor.after_seq,
                })
            && (!query.message_boundary_only
                || matches!(
                    self.role,
                    Some(AgentMessageRole::User | AgentMessageRole::Assistant)
                ))
    }
}
