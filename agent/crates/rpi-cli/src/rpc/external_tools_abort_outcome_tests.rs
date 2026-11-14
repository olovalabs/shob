// Authoritative settlement checks, beyond the `agent_settled` JSON projection.
async fn post_tool_external_control_abort_case(stop: bool) {
    use rpi_agent::AgentMessage;
    use rpi_harness::events::{RunEndEvent, RunEndOutcome};
    use rpi_harness::session::types::{Entry, LaneRecord, OperationOutcome};
    use tokio::io::AsyncWriteExt;

    let (harness, mut client, output, task, request) = external_pending_rpc().await;
    let run_ends = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ends = run_ends.clone();
    let _watch = harness.events().on::<RunEndEvent, _>(move |event| {
        ends.lock().unwrap().push(event.clone());
    });
    let control = if stop { "stop" } else { "abort" };
    client
        .write_all(format!("{{\"id\":3,\"type\":\"{control}\"}}\n").as_bytes())
        .await
        .unwrap();
    assert_eq!(output.wait_for(|m| m["id"] == 3).await["success"], true);
    let settled = output.wait_for(|m| m["type"] == "agent_settled").await;
    assert_eq!(settled["outcome"], "aborted", "{settled}");
    let run_id = settled["runId"].as_str().unwrap();
    assert!(run_ends
        .lock()
        .unwrap()
        .iter()
        .any(|end| end.run_id == run_id && end.outcome == RunEndOutcome::Aborted));
    let records = harness
        .session()
        .find_records(&Default::default())
        .await
        .unwrap();
    assert!(
        records.iter().any(
            |record| matches!(record, LaneRecord::OperationFinished(finished)
        if finished.run_id == run_id && finished.outcome == OperationOutcome::Aborted)
        ),
        "{records:?}"
    );
    let entries = harness
        .session()
        .find_entries(&rpi_harness::session::types::EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    let messages: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Message(entry) => Some(&entry.message),
            _ => None,
        })
        .collect();
    let assistant = messages
        .iter()
        .position(
            |m| matches!(m, AgentMessage::Assistant(a) if a.stop_reason == StopReason::ToolUse),
        )
        .unwrap();
    let result = messages
        .iter()
        .position(|m| matches!(m, AgentMessage::ToolResult(_)))
        .unwrap();
    let aborted = messages.iter().position(|m| matches!(m, AgentMessage::Assistant(a) if a.stop_reason == StopReason::Aborted && a.content.is_empty())).unwrap();
    assert!(
        assistant < result && result < aborted,
        "original call and settled result must precede the persisted abort: {messages:?}"
    );
    output
        .wait_for(|m| {
            m["type"] == "external_tool_request_closed" && m["requestId"] == request["requestId"]
        })
        .await;

    if !stop {
        let late = serde_json::json!({"id":4,"type":"external_tool_response","requestId":request["requestId"],"result":{"content":[]}});
        client
            .write_all(format!("{late}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(
            output.wait_for(|m| m["id"] == 4).await["data"]["accepted"],
            false
        );
        client
            .write_all(b"{\"id\":5,\"type\":\"prompt\",\"message\":\"next\"}\n")
            .await
            .unwrap();
        let next = output
            .wait_for(|m| m["type"] == "agent_settled" && m["runId"] != settled["runId"])
            .await;
        assert_eq!(
            next["outcome"], "completed",
            "next prompt must not inherit abort: {next}"
        );
        client.write_all(b"{\"type\":\"stop\"}\n").await.unwrap();
    }
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_tool_external_abort_has_durable_aborted_operation_and_run_end() {
    post_tool_external_control_abort_case(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_tool_external_stop_has_durable_aborted_operation_and_run_end() {
    post_tool_external_control_abort_case(true).await;
}
