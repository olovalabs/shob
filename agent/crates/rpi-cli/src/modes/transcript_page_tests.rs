//! RPC wire compatibility between cold JSONL pages and the legacy full-branch path.
use super::*;
use rpi_agent::message::{AgentMessage, CustomMessage};
use rpi_ai::types::{Api, AssistantMessage, StopReason};
use rpi_harness::session::{
    jsonl::{JsonlSessionStorage, JsonlV4Header},
    types::{ProvisionedEntry, ProvisionedKind, SessionStorage, SessionTree, TranscriptPageQuery},
    Clock, CounterIdGenerator, InMemorySessionStorage, Session,
};
use serde_json::{json, Value};

struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        42
    }
}

struct Fixture {
    // Keep the private synthetic session alive until all readers are dropped.
    jsonl: Arc<dyn SessionStorage>,
    memory: Arc<dyn SessionStorage>,
    temp: tempfile::TempDir,
}

fn user(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new(id, 42).into(),
            terminate: None,
        },
    }
}

impl Fixture {
    async fn new(messages: usize) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn rpi_tools::env::FileSystem> = Arc::new(
            rpi_tools::OsExecutionEnv::with_cwd(temp.path().to_path_buf()),
        );
        let clock: Arc<dyn Clock> = Arc::new(FixedClock);
        let ids = Arc::new(CounterIdGenerator::new());
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "transcript.jsonl",
            JsonlV4Header::new(
                "transcript-test".into(),
                42,
                "/synthetic".into(),
                None,
                None,
                None,
            ),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        let memory: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new(
            storage.metadata(),
            clock.clone(),
            ids.clone(),
        ));
        for index in 0..messages {
            let id = format!("message-{index}");
            let entry = match index {
                1 => {
                    let mut assistant = AssistantMessage::empty(
                        Api::AnthropicMessages,
                        "synthetic",
                        "test-model",
                        42,
                    );
                    assistant.stop_reason = StopReason::Stop;
                    ProvisionedEntry {
                        id,
                        kind: ProvisionedKind::Message {
                            message: assistant.into(),
                            terminate: None,
                        },
                    }
                }
                2 => ProvisionedEntry {
                    id,
                    kind: ProvisionedKind::Message {
                        message: serde_json::from_value(json!({
                            "kind":"tool_result", "role":"toolResult", "toolCallId":"call-1",
                            "toolName":"synthetic", "content":[], "isError":false, "timestamp":42
                        }))
                        .unwrap(),
                        terminate: None,
                    },
                },
                3 => ProvisionedEntry {
                    id,
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::Custom(CustomMessage::new(
                            "custom",
                            vec![],
                            json!({"customType":"notice"}),
                            42,
                        )),
                        terminate: None,
                    },
                },
                4 => ProvisionedEntry {
                    id,
                    kind: ProvisionedKind::Compaction {
                        summary: "synthetic summary".into(),
                        retained_tail: vec![rpi_ai::UserMessage::new("retained", 42).into()],
                        tokens_before: 123,
                        details: None,
                        usage: None,
                    },
                },
                _ => user(&id),
            };
            // Non-transcript entries are deliberately interspersed, including at the leaf.
            let metadata = ProvisionedEntry {
                id: format!("metadata-{index}"),
                kind: ProvisionedKind::Custom {
                    custom_type: "not-a-message".into(),
                    data: Some(json!({"index":index})),
                },
            };
            for entry in [entry, metadata] {
                storage.append_entry(entry.clone(), "main").await.unwrap();
                memory.append_entry(entry, "main").await.unwrap();
            }
        }
        storage.drain().await;
        drop(storage);
        // Exercise the real cold-reader metadata index, not just hot appended entries.
        let jsonl = Arc::new(
            JsonlSessionStorage::load(fs, "transcript.jsonl", clock, ids)
                .await
                .unwrap(),
        );
        Self {
            jsonl,
            memory,
            temp,
        }
    }

    async fn harness(storage: Arc<dyn SessionStorage>) -> AgentHarness {
        AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
            session: Session::new(storage, None),
            ..Default::default()
        })
        .await
        .unwrap()
    }

    async fn harnesses(&self) -> (AgentHarness, AgentHarness) {
        (
            Self::harness(self.jsonl.clone()).await,
            Self::harness(self.memory.clone()).await,
        )
    }
}

async fn get(harness: &AgentHarness, args: &Value) -> Result<Value, String> {
    rpc_get_messages(&harness.lane("main"), harness, args).await
}

fn message_ids(page: &Value) -> Vec<String> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["entryId"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn rpc_transcript_jsonl_matches_legacy_limits_cursors_and_projection() {
    let fixture = Fixture::new(405).await;
    let (jsonl, legacy) = fixture.harnesses().await;
    let query = TranscriptPageQuery {
        limit: 2,
        before_id: None,
    };
    assert!(jsonl.transcript_page(&query).await.unwrap().is_some());
    assert!(legacy.transcript_page(&query).await.unwrap().is_none());

    for args in [
        json!({}),
        json!({"beforeId":""}),
        json!({"limit":0}),
        json!({"limit":1}),
        json!({"limit":1000}),
        json!({"limit":3,"beforeId":"message-4"}),
        json!({"limit":3,"beforeId":"message-0"}),
    ] {
        let optimized = get(&jsonl, &args).await.unwrap();
        assert_eq!(optimized, get(&legacy, &args).await.unwrap(), "{args}");
        assert_eq!(optimized["total"], 405);
        assert_eq!(optimized["leafId"], "metadata-404");
        assert_eq!(optimized["pendingResume"], false);
        assert_eq!(optimized["truncated"], optimized["hasMore"]);
    }
    let default_page = get(&jsonl, &json!({})).await.unwrap();
    assert_eq!(default_page["limit"], 400);
    assert_eq!(message_ids(&default_page).len(), 400);
    assert_eq!(default_page["firstId"], "message-5");
    assert_eq!(default_page["hasMore"], true);
    let zero = get(&jsonl, &json!({"limit":0})).await.unwrap();
    assert_eq!(zero["limit"], 1);
    assert_eq!(message_ids(&zero), ["message-404"]);
    let first = get(&jsonl, &json!({"beforeId":"message-0"})).await.unwrap();
    assert!(message_ids(&first).is_empty());
    assert_eq!(first["firstId"], Value::Null);
    assert_eq!(first["lastId"], Value::Null);
    assert_eq!(first["hasMore"], false);
    let all = get(&jsonl, &json!({"limit":1000})).await.unwrap();
    let roles: Vec<_> = all["messages"].as_array().unwrap()[..5]
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "toolResult", "custom", "compaction"]
    );
    assert_eq!(all["messages"][4]["tokensBefore"], 123);
    assert_eq!(all["messages"][4]["retainedMessages"], 1);
    for cursor in ["unknown", "metadata-2"] {
        let args = json!({"limit":3,"beforeId":cursor});
        for harness in [&jsonl, &legacy] {
            let error = get(harness, &args).await.unwrap_err();
            assert_eq!(error, format!("beforeId not found: {cursor}"));
        }
    }
}

#[tokio::test]
async fn rpc_transcript_empty_jsonl_matches_legacy_fallback() {
    let fixture = Fixture::new(0).await;
    let (jsonl, legacy) = fixture.harnesses().await;
    for args in [json!({}), json!({"limit":0,"beforeId":""})] {
        let page = get(&jsonl, &args).await.unwrap();
        assert_eq!(page, get(&legacy, &args).await.unwrap());
        assert!(message_ids(&page).is_empty());
        assert_eq!(page["total"], 0);
        assert_eq!(page["leafId"], Value::Null);
        assert_eq!(page["firstId"], Value::Null);
        assert_eq!(page["lastId"], Value::Null);
        assert_eq!(page["hasMore"], false);
    }
}

#[tokio::test]
async fn rpc_transcript_pages_are_contiguous_without_duplicates() {
    let fixture = Fixture::new(23).await;
    let (jsonl, legacy) = fixture.harnesses().await;
    let mut args = json!({"limit":4});
    let mut reconstructed = Vec::new();
    loop {
        let page = get(&jsonl, &args).await.unwrap();
        assert_eq!(page, get(&legacy, &args).await.unwrap());
        let ids = message_ids(&page);
        assert!(!ids.is_empty());
        assert_eq!(page["firstId"], ids[0]);
        assert_eq!(page["lastId"], *ids.last().unwrap());
        reconstructed.splice(0..0, ids);
        if page["hasMore"] == false {
            break;
        }
        args["beforeId"] = page["firstId"].clone();
        assert!(reconstructed.len() < 23, "pagination failed to advance");
    }
    assert_eq!(
        reconstructed,
        (0..23)
            .map(|index| format!("message-{index}"))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn transcript_page_session_main_and_side_views_preserve_lane_snapshot() {
    let fixture = Fixture::new(6).await;
    for storage in [&fixture.jsonl, &fixture.memory] {
        storage
            .create_lane("side", Some("message-1"))
            .await
            .unwrap();
        storage
            .append_entry(user("side-only"), "side")
            .await
            .unwrap();
    }
    let query = TranscriptPageQuery {
        limit: 10,
        before_id: None,
    };
    let session = Session::new(fixture.jsonl.clone(), None);
    // Direct SessionTree dispatch and view("main") must both pass the main lane through.
    let main = SessionTree::transcript_page(&session, &query)
        .await
        .unwrap()
        .unwrap();
    let main_view = session
        .view("main")
        .transcript_page(&query)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(main.entries, main_view.entries);
    assert_eq!(main.total, 6);
    assert_eq!(main.leaf_id.as_deref(), Some("metadata-5"));
    let side = session
        .view("side")
        .transcript_page(&query)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(side.total, 3);
    assert_eq!(side.leaf_id.as_deref(), Some("side-only"));
    assert_eq!(
        side.entries
            .iter()
            .map(|entry| entry.id())
            .collect::<Vec<_>>(),
        ["message-0", "message-1", "side-only"]
    );
    assert!(!side.has_more);
    let bounded = session
        .view("side")
        .transcript_page(&TranscriptPageQuery {
            limit: 1,
            before_id: Some("side-only".into()),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        bounded
            .entries
            .iter()
            .map(|entry| entry.id())
            .collect::<Vec<_>>(),
        ["message-1"]
    );
    assert_eq!(bounded.total, 3);
    assert!(bounded.has_more);
    assert_eq!(bounded.leaf_id.as_deref(), Some("side-only"));
    let memory = Session::new(fixture.memory.clone(), None);
    assert!(SessionTree::transcript_page(&memory, &query)
        .await
        .unwrap()
        .is_none());
    assert!(memory
        .view("side")
        .transcript_page(&query)
        .await
        .unwrap()
        .is_none());

    // A separately moving lane must not replace the storage page's pinned leaf.
    let harness = Fixture::harness(fixture.jsonl.clone()).await;
    let page = rpc_get_messages(&harness.lane("side"), &harness, &json!({}))
        .await
        .unwrap();
    assert_eq!(page["leafId"], "metadata-5");
    assert!(!message_ids(&page).contains(&"side-only".to_owned()));
}

#[tokio::test]
async fn rpc_transcript_cold_jsonl_errors_do_not_retry_legacy_projection() {
    let fixture = Fixture::new(1).await;
    let harness = Fixture::harness(fixture.jsonl.clone()).await;
    let path = fixture.temp.path().join("transcript.jsonl");
    let original = std::fs::read_to_string(&path).unwrap();
    // Same-length corruption keeps every indexed byte range valid. The metadata
    // still knows the branch and cursor, but hydrating the selected payload fails.
    let corrupt = original.replace("\"kind\":\"user\"", "\"kind\":\"nope\"");
    assert_ne!(original, corrupt);
    assert_eq!(original.len(), corrupt.len());
    std::fs::write(&path, corrupt).unwrap();
    assert!(get(&harness, &json!({"limit":1})).await.is_err());
    // A fallback would hydrate the corrupt branch and replace this cursor error
    // with a decoding error. Preserve the page-selection error instead.
    let error = get(&harness, &json!({"beforeId":"absent"}))
        .await
        .unwrap_err();
    assert!(error.contains("beforeId not found: absent"), "{error}");
}

#[tokio::test]
async fn rpc_state_bounded_usage_matches_legacy_full_wire_state() {
    for count in [0, 1, 9] {
        let fixture = Fixture::new(count).await;
        let (cold, legacy) = fixture.harnesses().await;
        assert!(cold.context_usage().await.unwrap().is_some());
        assert!(legacy.context_usage().await.unwrap().is_none());
        let cold_lane = cold.lane("main");
        let legacy_lane = legacy.lane("main");
        assert_eq!(
            rpc_get_state(&cold_lane, &cold).await,
            rpc_get_state(&legacy_lane, &legacy).await,
            "count={count}"
        );
    }
}

#[tokio::test]
async fn rpc_state_keeps_best_effort_usage_and_does_not_read_snapshot_labels() {
    let fixture = Fixture::new(1).await;
    let harness = Fixture::harness(fixture.jsonl.clone()).await;
    let lane = harness.lane("main");
    let path = fixture.temp.path().join("transcript.jsonl");
    let original = std::fs::read_to_string(&path).unwrap();
    let corrupt = original.replace("\"kind\":\"user\"", "\"kind\":\"nope\"");
    assert_ne!(original, corrupt);
    assert_eq!(original.len(), corrupt.len());
    std::fs::write(&path, corrupt).unwrap();
    // Snapshot label hydration remains best effort on corrupt data. Reading
    // active state must preserve its value without requiring that projection.
    let snapshot = lane.snapshot().await.unwrap();
    assert!(snapshot.labels.is_empty());
    assert!(snapshot.active.is_none());
    assert!(lane.active_operation().await.unwrap().is_none());
    assert!(harness.context_usage().await.is_err());
    let state = rpc_get_state(&lane, &harness).await;
    assert_eq!(state["context"]["tokens"], 0);
    assert_eq!(state["context"]["trailingTokens"], 0);
    assert_eq!(state["context"]["estimated"], true);
    assert_eq!(state["leafId"], "metadata-0");
    assert_eq!(state["activeOperation"], Value::Null);
    assert_eq!(state["stats"]["messageCount"], 1);
}

#[tokio::test]
async fn context_usage_supported_backend_forwards_main_side_and_invalid_lane() {
    let fixture = Fixture::new(1).await;
    fixture
        .jsonl
        .create_lane("side", Some("metadata-0"))
        .await
        .unwrap();
    let mut assistant =
        AssistantMessage::empty(Api::AnthropicMessages, "synthetic", "test-model", 42);
    assistant.stop_reason = StopReason::Stop;
    assistant.usage.input = 111;
    fixture
        .jsonl
        .append_entry(
            ProvisionedEntry {
                id: "side-usage".into(),
                kind: ProvisionedKind::Message {
                    message: assistant.into(),
                    terminate: None,
                },
            },
            "side",
        )
        .await
        .unwrap();
    let main = fixture.jsonl.context_usage("main").await.unwrap().unwrap();
    let side = fixture.jsonl.context_usage("side").await.unwrap().unwrap();
    assert_eq!(side.tokens, 111);
    assert_ne!(main.tokens, side.tokens);
    let session = Session::new(fixture.jsonl.clone(), None);
    assert_eq!(session.context_usage().await.unwrap(), Some(main.clone()));
    assert_eq!(
        SessionTree::context_usage(&session).await.unwrap(),
        Some(main.clone())
    );
    assert_eq!(
        session.view("main").context_usage().await.unwrap(),
        Some(main.clone())
    );
    assert_eq!(
        session.view("side").context_usage().await.unwrap(),
        Some(side)
    );
    assert!(fixture.jsonl.context_usage("missing").await.is_err());
    assert!(session.view("missing").context_usage().await.is_err());
    let harness = Fixture::harness(fixture.jsonl.clone()).await;
    assert_eq!(harness.context_usage().await.unwrap(), Some(main));
}
