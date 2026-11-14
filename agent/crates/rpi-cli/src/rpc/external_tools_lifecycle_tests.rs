async fn external_pending_rpc() -> (
    AgentHarness,
    tokio::io::DuplexStream,
    RpcOutput,
    tokio::task::JoinHandle<i32>,
    serde_json::Value,
) {
    let harness = external_pending_harness().await;
    let (mut client, output, task) =
        start_test_rpc(harness.clone(), rpi_extensions::UiDialogMailbox::new());
    let request = external_start_pending(&mut client, &output).await;
    (harness, client, output, task, request)
}

async fn external_pending_harness() -> AgentHarness {
    use rpi_ai::providers::faux::{
        faux_assistant_message, FauxBlock, FauxProvider, FauxScript, FauxStep,
    };
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::factory(|_| {
            faux_assistant_message(
                vec![FauxBlock::tool_call(
                    "remote_lookup",
                    serde_json::json!({"query":"hold"}),
                )],
                rpi_ai::types::StopReason::ToolUse,
            )
        }),
        FauxStep::factory(|_| faux_assistant_message("done", rpi_ai::types::StopReason::Stop)),
    ]);
    let provider = FauxProvider::new(script);
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    harness
}

async fn external_start_pending(
    client: &mut tokio::io::DuplexStream,
    output: &RpcOutput,
) -> serde_json::Value {
    use tokio::io::AsyncWriteExt;
    let config = serde_json::json!({"id":1,"type":"configure_external_tools","tools":[external_definition("remote_lookup")]});
    client
        .write_all(format!("{config}\n").as_bytes())
        .await
        .unwrap();
    assert_eq!(output.wait_for(|m| m["id"] == 1).await["success"], true);
    client
        .write_all(b"{\"id\":2,\"type\":\"prompt\",\"message\":\"lookup\"}\n")
        .await
        .unwrap();
    output
        .wait_for(|m| m["type"] == "external_tool_request")
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_eof_resolves_outstanding_and_disables_registry_backend() {
    use tokio::io::AsyncWriteExt;
    let (harness, mut client, output, task, request) = external_pending_rpc().await;
    client.shutdown().await.unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(output
        .messages()
        .iter()
        .any(|m| m["type"] == "external_tool_request_closed"
            && m["requestId"] == request["requestId"]));
    // Retained registry entries must neither hang nor keep the writer alive.
    let tools = harness.get_tools().await.unwrap();
    let tool = &tools[0].tool;
    let result = tool
        .execute(
            "after-eof",
            serde_json::json!({"query":"x"}),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await;
    assert!(matches!(result, Err(rpi_agent::error::AgentError::Abort)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_owner_drop_disables_retained_backend() {
    external_transport::owner_drop_witness().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_admitted_prompt_rejects_configuration_before_lane_entry() {
    use tokio::io::AsyncWriteExt;
    let (harness, _) = rpc_test_harness("one two three four five", 1.0).await;
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
    client.write_all(b"{\"id\":1,\"type\":\"prompt\",\"message\":\"test\"}\n{\"id\":2,\"type\":\"configure_external_tools\",\"tools\":[]}\n{\"type\":\"stop\"}\n").await.unwrap();
    assert_eq!(output.wait_for(|m| m["id"] == 2).await["success"], false);
    assert_eq!(task.await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_healthy_eof_has_no_two_second_watchdog() {
    use tokio::io::AsyncWriteExt;
    let (harness, _) = rpc_test_harness("one two three", 1.0).await;
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
    client
        .write_all(b"{\"id\":1,\"type\":\"prompt\",\"message\":\"test\"}\n")
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    let start = std::time::Instant::now();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(start.elapsed() > std::time::Duration::from_secs(2));
    assert_eq!(
        output.wait_for(|m| m["type"] == "agent_settled").await["outcome"],
        "completed"
    );
}

include!("external_tools_transport_tests.rs");
