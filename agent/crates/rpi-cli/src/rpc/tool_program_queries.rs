//! Native read-only Code Mode child inspection RPC mappings.
//!
//! The harness owns validation, branch binding, pinning, and bounded page
//! production. This module only removes the two RPC envelope fields and maps
//! the resulting typed page to JSON.

use rpi_harness::{
    agent_harness::AgentHarness,
    session::session::SessionEntryCommit,
    tool_program_queries::{
        ChildBodyPage, ChildBodyQuery, ChildPage, ChildPageQuery, ChildPhasePage, ChildPhaseQuery,
    },
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

const CHILD_BEGIN_CUSTOM_TYPE: &str = "rpi.tool_program.child_begin.v1";
const CHILD_PHASE_CUSTOM_TYPE: &str = "rpi.tool_program.child_phase.v1";
pub(crate) const CHILD_EVENT_MAX_BYTES: usize = super::protocol::TOOL_PROGRAM_CHILD_EVENT_MAX_BYTES;

/// Project a commit without applying the child-event size budget to ordinary
/// invalidations. Native journal events are the only events with a bounded
/// inline payload and a resync fallback.
pub(crate) fn commit_event(commit: SessionEntryCommit) -> Value {
    let Some(native) = commit.native_journal else {
        return json!({
            "type": "session_entry_committed",
            "entryId": commit.entry_id,
            "entryType": commit.entry_type,
            "customType": commit.custom_type,
            "lane": commit.lane,
        });
    };

    let event_type = match native.custom_type.as_str() {
        CHILD_BEGIN_CUSTOM_TYPE => "tool_program_child_begin",
        CHILD_PHASE_CUSTOM_TYPE => "tool_program_child_phase",
        _ => {
            return json!({
                "type": "tool_program_child_resync_required",
                "reason": "unknown_native_journal_custom_type",
                "skipped": 1,
            });
        }
    };
    let event = json!({
        "type": event_type,
        "schemaVersion": native.schema_version,
        "sourceSessionId": native.source_session_id,
        "lane": native.lane,
        "entryId": native.entry_id,
        "parentEntryId": native.parent_entry_id,
        "seq": native.seq,
        "customType": native.custom_type,
        "data": native.data,
    });
    if serde_json::to_vec(&event)
        .map(|bytes| bytes.len() <= CHILD_EVENT_MAX_BYTES)
        .unwrap_or(false)
    {
        event
    } else {
        json!({
            "type": "tool_program_child_resync_required",
            "reason": "native_journal_event_too_large",
            "skipped": 1,
        })
    }
}

pub(crate) fn commit_stream_event(
    result: Result<SessionEntryCommit, tokio::sync::broadcast::error::RecvError>,
) -> Option<Value> {
    match result {
        Ok(commit) => Some(commit_event(commit)),
        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => Some(json!({
            "type": "tool_program_child_resync_required",
            "reason": "commit_stream_lagged",
            "skipped": skipped,
        })),
        Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
    }
}

fn query<T: DeserializeOwned>(message: &Value) -> Result<T, String> {
    let mut body = message
        .as_object()
        .cloned()
        .ok_or_else(|| "query must be an object".to_owned())?;
    // Correlation and dispatch metadata are the only fields owned by RPC.
    // Everything else must be accepted by the strict harness DTO unchanged.
    body.remove("type");
    body.remove("id");
    serde_json::from_value(Value::Object(body)).map_err(|error| error.to_string())
}

fn page<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

pub(crate) async fn branch(harness: &AgentHarness, message: &Value) -> Result<Value, String> {
    let object = message
        .as_object()
        .ok_or_else(|| "branch request must be an object".to_owned())?;
    if object.keys().any(|key| key != "type" && key != "id") {
        return Err("get_tool_program_branch accepts only type and id".into());
    }
    let result = harness
        .session()
        .tool_program_branch(harness.lane_name())
        .await
        .map_err(|error| error.to_string())?;
    page(result)
}

pub(crate) async fn children(harness: &AgentHarness, message: &Value) -> Result<Value, String> {
    let query: ChildPageQuery = query(message)?;
    let result: ChildPage = rpi_harness::tool_program_queries::read_child_page(
        harness.session(),
        harness.lane_name(),
        &query,
    )
    .await
    .map_err(|error| error.to_string())?;
    page(result)
}

pub(crate) async fn phases(harness: &AgentHarness, message: &Value) -> Result<Value, String> {
    let query: ChildPhaseQuery = query(message)?;
    let result: ChildPhasePage = rpi_harness::tool_program_queries::read_child_phase_page(
        harness.session(),
        harness.lane_name(),
        &query,
    )
    .await
    .map_err(|error| error.to_string())?;
    page(result)
}

pub(crate) async fn body(harness: &AgentHarness, message: &Value) -> Result<Value, String> {
    let query: ChildBodyQuery = query(message)?;
    let result: ChildBodyPage = rpi_harness::tool_program_queries::read_child_body_page(
        harness.session(),
        harness.lane_name(),
        &query,
    )
    .await
    .map_err(|error| error.to_string())?;
    page(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_harness::tool_program_queries::{ChildCursor, Direction};
    use serde_json::json;

    #[tokio::test]
    async fn branch_identity_profile_and_strict_rpc_arguments() {
        let ready = super::super::protocol::ready(crate::args::RpcMessageUpdates::CompactDeltas);
        assert_eq!(
            ready["toolProgramBranch"],
            json!({"version":1,
            "command":"get_tool_program_branch","identity":"bindingAndLaneGeneration"})
        );
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let initial = branch(
            &harness,
            &json!({"type":"get_tool_program_branch","id":"one"}),
        )
        .await
        .unwrap();
        assert_eq!(initial["lane"], "main");
        assert_eq!(initial["generation"], 0);
        let binding = uuid::Uuid::parse_str(initial["bindingId"].as_str().unwrap()).unwrap();
        assert_eq!(binding.get_version_num(), 4);
        for args in [
            json!(null),
            json!({"leafId":null}),
            json!({"lane":"main"}),
            json!({"generation":0}),
        ] {
            assert!(branch(&harness, &args).await.is_err());
        }
    }

    #[test]
    fn strips_only_rpc_envelope_and_preserves_query_fields() {
        let parsed: ChildPageQuery = query(&json!({
            "type": "get_tool_program_children",
            "id": "request-1",
            "leafId": "leaf-1",
            "direction": "newestFirst",
            "limit": 7,
            "maxBytes": 4096
        }))
        .unwrap();
        assert_eq!(parsed.leaf_id.as_deref(), Some("leaf-1"));
        assert_eq!(parsed.direction, Direction::NewestFirst);
        assert_eq!(parsed.limit, 7);
        assert_eq!(parsed.max_bytes, 4096);
    }

    #[test]
    fn strict_query_rejects_unknown_fields_and_missing_required_parameters() {
        assert!(query::<ChildPageQuery>(&json!({
            "direction": "oldestFirst",
            "limit": 1,
            "maxBytes": 128,
            "unexpected": true
        }))
        .is_err());
        assert!(query::<ChildPageQuery>(&json!({
            "limit": 1,
            "maxBytes": 128
        }))
        .is_err());
    }

    #[test]
    fn nested_cursor_is_strict_but_round_trips_as_typed_data() {
        let cursor = ChildCursor {
            session_id: "session".into(),
            lane: "main".into(),
            leaf_id: "leaf".into(),
            direction: Direction::OldestFirst,
            child_id: None,
            after_entry_id: "entry".into(),
            after_seq: 9,
        };
        let parsed: ChildPageQuery = query(&json!({
            "direction": "oldestFirst",
            "limit": 1,
            "maxBytes": 128,
            "cursor": cursor
        }))
        .unwrap();
        assert_eq!(parsed.cursor.unwrap().after_seq, 9);
        assert!(query::<ChildPageQuery>(&json!({
            "direction": "oldestFirst",
            "limit": 1,
            "maxBytes": 128,
            "cursor": {
                "sessionId": "session",
                "lane": "main",
                "leafId": "leaf",
                "direction": "oldestFirst",
                "afterEntryId": "entry",
                "afterSeq": 9,
                "extra": true
            }
        }))
        .is_err());
    }

    #[test]
    fn all_queries_require_native_parameters_and_reject_rpc_extras() {
        let cases: [(Value, fn(&Value) -> bool, &[&str]); 3] = [
            (
                json!({"direction":"oldestFirst", "limit":2, "maxBytes":1024}),
                |v| query::<ChildPageQuery>(v).is_ok(),
                &["direction", "limit", "maxBytes"],
            ),
            (
                json!({"childId":"child", "direction":"newestFirst", "limit":3, "maxBytes":2048}),
                |v| query::<ChildPhaseQuery>(v).is_ok(),
                &["childId", "direction", "limit", "maxBytes"],
            ),
            (
                json!({"childId":"child", "phaseEntryId":"phase", "descriptorId":"descriptor", "limit":4, "maxBytes":4096}),
                |v| query::<ChildBodyQuery>(v).is_ok(),
                &[
                    "childId",
                    "phaseEntryId",
                    "descriptorId",
                    "limit",
                    "maxBytes",
                ],
            ),
        ];
        for (mut value, parse, required) in cases {
            value["type"] = json!("rpc-command");
            value["id"] = json!({"opaque": [1, 2]});
            assert!(parse(&value));
            for key in required {
                let mut missing = value.clone();
                missing.as_object_mut().unwrap().remove(*key);
                assert!(!parse(&missing), "missing {key}");
            }
            for key in ["sessionId", "lane", "offset", "params", "version"] {
                let mut extra = value.clone();
                extra[key] = json!(1);
                assert!(!parse(&extra), "unknown {key}");
            }
        }
    }

    #[test]
    fn phase_and_body_queries_preserve_all_native_fields() {
        let phase = json!({"childId":"child", "leafId":"leaf", "cursor":null,
            "direction":"newestFirst", "limit":3, "maxBytes":2048});
        assert_eq!(
            serde_json::to_value(query::<ChildPhaseQuery>(&phase).unwrap()).unwrap(),
            phase
        );
        let body = json!({"childId":"child", "phaseEntryId":"phase", "descriptorId":"descriptor",
            "leafId":"leaf", "cursor": {"direction":"oldestFirst", "childId":"child", "phaseEntryId":"phase",
                "body":{"sessionId":"session", "lane":"lane", "leafId":"leaf", "descriptorId":"descriptor", "nextIndex":2}},
            "limit":4, "maxBytes":4096});
        assert_eq!(
            serde_json::to_value(query::<ChildBodyQuery>(&body).unwrap()).unwrap(),
            body
        );
    }

    #[test]
    fn complete_typed_page_fields_are_not_projected_away() {
        let expected = json!({"leafId":"leaf", "childId":"child", "phases":[],
            "nextCursor":null, "scanned":128, "scanExhausted":false, "state":"unknown"});
        let typed: ChildPhasePage = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(page(typed).unwrap(), expected);
    }

    fn commit(
        native_journal: Option<rpi_harness::session::session::NativeJournalCommit>,
    ) -> SessionEntryCommit {
        SessionEntryCommit {
            entry_id: "entry".into(),
            entry_type: "custom".into(),
            custom_type: Some("ordinary".into()),
            lane: "main".into(),
            native_journal,
        }
    }

    fn native(
        custom_type: &str,
        data: Value,
    ) -> rpi_harness::session::session::NativeJournalCommit {
        rpi_harness::session::session::NativeJournalCommit {
            schema_version: 1,
            source_session_id: "source".into(),
            lane: "main".into(),
            entry_id: "native-entry".into(),
            parent_entry_id: Some("parent".into()),
            seq: 7,
            custom_type: custom_type.into(),
            data,
        }
    }

    #[test]
    fn native_event_projection_bounds_only_dedicated_events() {
        let ordinary = commit(None);
        let ordinary_event = commit_event(SessionEntryCommit {
            custom_type: Some("x".repeat(100_000)),
            ..ordinary
        });
        assert_eq!(ordinary_event["type"], "session_entry_committed");
        assert_eq!(
            ordinary_event["customType"].as_str().unwrap().len(),
            100_000
        );

        let begin = commit_event(commit(Some(native(
            CHILD_BEGIN_CUSTOM_TYPE,
            json!({"ok": true}),
        ))));
        assert_eq!(begin["type"], "tool_program_child_begin");
        assert_eq!(begin["sourceSessionId"], "source");
        assert_eq!(begin["seq"], 7);

        let unknown = commit_event(commit(Some(native(
            "rpi.tool_program.unknown.v1",
            json!({}),
        ))));
        assert_eq!(unknown["type"], "tool_program_child_resync_required");
        assert_eq!(unknown["reason"], "unknown_native_journal_custom_type");

        let oversized = commit_event(commit(Some(native(
            CHILD_PHASE_CUSTOM_TYPE,
            json!({
                "payload": "x".repeat(CHILD_EVENT_MAX_BYTES),
            }),
        ))));
        assert_eq!(oversized["type"], "tool_program_child_resync_required");
        assert_eq!(oversized["reason"], "native_journal_event_too_large");
    }

    #[test]
    fn commit_stream_mapping_resyncs_lag_and_keeps_native_order() {
        let first = commit_stream_event(Ok(commit(Some(native(
            CHILD_BEGIN_CUSTOM_TYPE,
            json!({"index": 0}),
        )))))
        .unwrap();
        let second = commit_stream_event(Ok(commit(Some(native(
            CHILD_PHASE_CUSTOM_TYPE,
            json!({"index": 1}),
        )))))
        .unwrap();
        assert_eq!(first["type"], "tool_program_child_begin");
        assert_eq!(second["type"], "tool_program_child_phase");
        assert_eq!(first["seq"], 7);
        assert_eq!(second["seq"], 7);
        let lagged =
            commit_stream_event(Err(tokio::sync::broadcast::error::RecvError::Lagged(3))).unwrap();
        assert_eq!(lagged["type"], "tool_program_child_resync_required");
        assert_eq!(lagged["skipped"], 3);
        assert!(
            commit_stream_event(Err(tokio::sync::broadcast::error::RecvError::Closed,)).is_none()
        );
    }

    #[tokio::test]
    async fn actual_broadcast_lag_requires_resync_before_retained_ordered_commits() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(2);
        for seq in 1..=4 {
            let mut payload = native(CHILD_PHASE_CUSTOM_TYPE, json!({"index": seq}));
            payload.seq = seq;
            tx.send(commit(Some(payload))).unwrap();
        }
        let lag = commit_stream_event(rx.recv().await).unwrap();
        assert_eq!(lag["type"], "tool_program_child_resync_required");
        assert_eq!(lag["skipped"], 2);
        for seq in 3..=4 {
            let event = commit_stream_event(rx.recv().await).unwrap();
            assert_eq!(event["type"], "tool_program_child_phase");
            assert_eq!(event["seq"], seq);
        }
        drop(tx);
        assert!(commit_stream_event(rx.recv().await).is_none());
    }

    #[test]
    fn non_object_envelope_is_rejected() {
        assert!(query::<ChildPageQuery>(&Value::Null).is_err());
    }
}
