#[tokio::test]
async fn native_tool_result_with_null_details_roundtrips() {
    let temp = tempfile::tempdir().unwrap();
    use rpi_harness::session::{experimental_native::ExperimentalNativeRepo, types::*};
    let repo = ExperimentalNativeRepo::new(temp.path().to_owned())
        .await
        .unwrap();
    let s = repo.create(&SessionCreateOptions::default()).await.unwrap();
    let mut message: rpi_agent::AgentMessage=serde_json::from_value(serde_json::json!({"kind":"tool_result","role":"toolResult","toolCallId":"fixture-tool","toolName":"read","content":[{"type":"text","text":"tool result from native fixture"}],"details":null,"isError":false,"timestamp":42})).unwrap();
    if let rpi_agent::AgentMessage::ToolResult(result) = &mut message {
        result.details = Some(serde_json::Value::Null);
    }
    let expected = message.clone();
    s.append_entry(
        ProvisionedEntry {
            id: "tool-result".into(),
            kind: ProvisionedKind::Message {
                message,
                terminate: None,
            },
        },
        "main",
    )
    .await
    .unwrap();
    let Entry::Message(entry) = s.get_entry("tool-result").await.unwrap().unwrap() else {
        panic!("not message")
    };
    assert_eq!(entry.message, expected);
}
