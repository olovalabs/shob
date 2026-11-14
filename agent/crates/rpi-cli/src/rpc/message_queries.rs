//! Optional bounded branch transcript reads, independent of UI/HTTP formats.
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::types::{Entry, MessageSummaryQuery, MessageWindowQuery};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
fn query<T: DeserializeOwned>(msg: &Value) -> Result<T, String> {
    let mut body = msg.as_object().ok_or("query must be an object")?.clone();
    body.remove("type");
    body.remove("id");
    serde_json::from_value(Value::Object(body)).map_err(|error| error.to_string())
}
pub(crate) async fn summaries(harness: &AgentHarness, msg: &Value) -> Result<Value, String> {
    let query: MessageSummaryQuery = query(msg)?;
    let result = harness
        .message_summaries(&query)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::to_value(result).map_err(|error| error.to_string())
}
pub(crate) async fn window(harness: &AgentHarness, msg: &Value) -> Result<Value, String> {
    let query: MessageWindowQuery = query(msg)?;
    let page = harness
        .message_window(&query)
        .await
        .map_err(|error| error.to_string())?;
    let messages = page.entries.iter().map(|entry| {
        let mut value = match entry {
            Entry::Message(message) => serde_json::to_value(&message.message).map_err(|error| error.to_string())?,
            Entry::Compaction(compaction) => json!({"role":"compaction", "tokensBefore":compaction.tokens_before, "retainedMessages":compaction.retained_tail.len(), "timestamp":compaction.base.timestamp}),
            _ => return Err("unexpected entry in message window".into()),
        };
        value["entryId"] = json!(entry.id());
        Ok(value)
    }).collect::<Result<Vec<_>, String>>()?;
    Ok(
        json!({"messages":messages, "firstId":page.first_id, "lastId":page.last_id,
        "nextBeforeId":page.next_before_id, "nextAfterId":page.next_after_id,
        "hasMoreBefore":page.has_more_before, "hasMoreAfter":page.has_more_after,
        "leafId":page.leaf_id}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_defaults_and_cursors_survive_rpc_envelope() {
        let parsed: MessageSummaryQuery = query(
            &json!({"type":"get_message_summaries", "id":"req", "beforeId":"u1", "leafId":"leaf"}),
        )
        .unwrap();
        assert_eq!(parsed.limit, 10);
        assert_eq!(parsed.role, "user");
        assert_eq!(parsed.before_id.as_deref(), Some("u1"));
        assert_eq!(parsed.leaf_id.as_deref(), Some("leaf"));
        assert!(query::<MessageSummaryQuery>(&json!({"limit":-1})).is_err());
        assert!(query::<MessageSummaryQuery>(&json!({"beforeID":"typo"})).is_err());
    }
    #[test]
    fn context_supports_each_cursor_without_silently_accepting_unknown_fields() {
        for cursor in ["anchorId", "beforeId", "afterId"] {
            let mut value = json!({"type":"get_message_window","id":1,"leafId":"snapshot"});
            value[cursor] = json!("u1");
            let parsed: MessageWindowQuery = query(&value).unwrap();
            assert_eq!(parsed.limit, 48);
            assert_eq!(parsed.leaf_id.as_deref(), Some("snapshot"));
        }
        assert!(query::<MessageWindowQuery>(&json!({"offset":999})).is_err());
    }
}
