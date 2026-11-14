//! Canonical JSONL bytes with native typed-presence restoration. The legacy
//! decoder uses Option<Value> (missing and explicit null both decode to None).
//! Native ACKs promise exact typed roundtrips, so restore supported opaque JSON
//! fields from the already schema-validated, budget-bounded canonical object.
//! This does not change the production JSONL parser or its wire format.
use super::{
    engine::Result,
    facts::{jsonl, types::*},
};
use serde_json::Value;

pub(super) fn parse(line: &str) -> Result<SessionMutation> {
    let mut mutation = jsonl::parse_mutation(line)?;
    let value: Value = serde_json::from_str(line)?;
    match &mut mutation {
        SessionMutation::Entry { entry, .. } => match entry {
            Entry::Message(message) => {
                if let rpi_agent::AgentMessage::ToolResult(tool) = &mut message.message {
                    if value.get("message").and_then(|m| m.get("details")) == Some(&Value::Null) {
                        tool.details = Some(Value::Null);
                    }
                }
            }
            Entry::Custom(entry) => {
                if value.get("data") == Some(&Value::Null) {
                    entry.data = Some(Value::Null);
                }
            }
            Entry::Compaction(entry) => {
                if value.get("details") == Some(&Value::Null) {
                    entry.details = Some(Value::Null);
                }
            }
            Entry::BranchSummary(entry) => {
                if value.get("details") == Some(&Value::Null) {
                    entry.details = Some(Value::Null);
                }
            }
            _ => {}
        },
        SessionMutation::Record {
            record: LaneRecord::AssistantFrame(frame),
        } => {
            if value.get("frame") == Some(&Value::Null) {
                frame.frame = Some(Value::Null);
            }
        }
        _ => {}
    }
    Ok(mutation)
}
