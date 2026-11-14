fn external_definition(name: &str) -> serde_json::Value {
    serde_json::json!({"name":name,"description":"Remote lookup","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_configuration_is_atomic_and_preserves_native_active_selection() {
    use tokio::io::AsyncWriteExt;
    let (harness, _) = rpc_test_harness("unused", 0.0).await;
    // Install actual registry entries, including a plugin name not in the
    // built-in denylist, rather than testing only the reported active names.
    let (writer, writer_task, _) =
        crate::rpc::writer::start_rpc_writer(std::io::sink(), Default::default());
    let backend = crate::rpc::external_tools::ExternalTools::detached(writer.clone());
    let tools = ["read", "write", "plugin_native"]
        .into_iter()
        .map(|name| {
            let def = serde_json::from_value(external_definition(name)).unwrap();
            rpi_harness::types::HarnessTool::new(rpi_tools::tools::external::create_external_tool(
                def,
                backend.clone(),
            ))
        })
        .collect();
    harness.set_tools(tools, None).await.unwrap();
    backend.shutdown();
    drop(backend);
    drop(writer);
    let (mut client, output, task) =
        start_test_rpc(harness.clone(), rpi_extensions::UiDialogMailbox::new());
    async fn command(
        client: &mut tokio::io::DuplexStream,
        output: &RpcOutput,
        id: u64,
        mut value: serde_json::Value,
    ) -> serde_json::Value {
        value["id"] = serde_json::json!(id);
        client
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        output.wait_for(|m| m["id"] == id).await
    }
    let configured = command(&mut client, &output, 1, serde_json::json!({"type":"configure_external_tools","tools":[external_definition("remote_lookup")]})).await;
    assert_eq!(configured["success"], true);
    assert!(harness
        .get_tools()
        .await
        .unwrap()
        .iter()
        .any(|t| t.tool.schema().name == "remote_lookup"));
    for (i, bad) in ["write", "permissions", "plugin_native"].iter().enumerate() {
        let response = command(&mut client, &output, 10 + i as u64, serde_json::json!({"type":"configure_external_tools","tools":[external_definition(bad)]})).await;
        assert_eq!(response["success"], false, "{response}");
        assert!(harness
            .get_tools()
            .await
            .unwrap()
            .iter()
            .any(|t| t.tool.schema().name == "remote_lookup"));
    }
    assert_eq!(
        command(
            &mut client,
            &output,
            24,
            serde_json::json!({"type":"configure_external_tools","tools":[]})
        )
        .await["success"],
        true
    );
    assert_eq!(
        harness.lane("main").get_active_tools().await.unwrap(),
        vec!["read", "write", "plugin_native"]
    );
    assert!(!harness
        .get_tools()
        .await
        .unwrap()
        .iter()
        .any(|t| t.tool.schema().name == "remote_lookup"));
    client.write_all(b"{\"type\":\"stop\"}\n").await.unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap(),
        0
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), writer_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_configuration_rejected_while_prompt_active() {
    use tokio::io::AsyncWriteExt;
    let (harness, provider) = rpc_test_harness("a long running response", 1.0).await;
    let (mut client, output, task) =
        start_test_rpc(harness.clone(), rpi_extensions::UiDialogMailbox::new());
    client
        .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
        .await
        .unwrap();
    // This helper intentionally has no event pump. Wait for the actual
    // provider invocation, not an event that cannot be forwarded.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let msg = serde_json::json!({"id":2,"type":"configure_external_tools","tools":[external_definition("remote_lookup")]});
    client
        .write_all(format!("{msg}\n").as_bytes())
        .await
        .unwrap();
    let response = output.wait_for(|m| m["id"] == 2).await;
    assert_eq!(response["success"], false);
    assert!(response.to_string().contains("active"));
    assert!(harness.get_tools().await.unwrap().is_empty());
    client.write_all(b"{\"type\":\"stop\"}\n").await.unwrap();
    assert_eq!(task.await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_model_registry_and_image_result_roundtrip() {
    use rpi_ai::providers::faux::{
        faux_assistant_message, FauxBlock, FauxProvider, FauxScript, FauxStep,
    };
    use tokio::io::AsyncWriteExt;
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::factory(|context| {
            assert_eq!(context.tools.len(), 1);
            let tool = &context.tools[0];
            assert_eq!(tool.name, "remote_lookup");
            assert_eq!(
                serde_json::to_value(&tool.parameters).unwrap(),
                external_definition("remote_lookup")["inputSchema"]
            );
            faux_assistant_message(
                vec![
                    FauxBlock::tool_call("remote_lookup", serde_json::json!({})),
                    FauxBlock::tool_call("remote_lookup", serde_json::json!({"query":"hello"})),
                ],
                rpi_ai::types::StopReason::ToolUse,
            )
        }),
        FauxStep::factory(|context| {
            let messages = serde_json::to_value(&context.messages).unwrap().to_string();
            assert!(messages.contains("image/png"), "{messages}");
            assert!(messages.contains("iVBORw0KGgo"), "{messages}");
            assert!(messages.contains("remote answer"), "{messages}");
            assert!(messages.contains("schema error"), "{messages}");
            // Inspect provider-visible content, not transcript details: the
            // structured object must survive the native tool-result boundary.
            let values = serde_json::to_value(&context.messages).unwrap();
            let result = values.as_array().unwrap().last().unwrap();
            let blocks = result["content"].as_array().unwrap();
            let structured: Vec<_> = blocks
                .iter()
                .filter_map(|block| {
                    serde_json::from_str::<serde_json::Value>(block["text"].as_str()?).ok()
                })
                .collect();
            assert_eq!(structured, vec![serde_json::json!({"preserved":true})]);
            assert!(!result["content"].to_string().contains("private-metadata"));
            assert_eq!(context.tools.len(), 1);
            assert_eq!(context.tools[0].name, "remote_lookup");
            assert_eq!(
                serde_json::to_value(&context.tools[0].parameters).unwrap(),
                external_definition("remote_lookup")["inputSchema"]
            );
            faux_assistant_message(
                vec![FauxBlock::tool_call(
                    "remote_lookup",
                    serde_json::json!({"query":"error"}),
                )],
                rpi_ai::types::StopReason::ToolUse,
            )
        }),
        FauxStep::factory(|context| {
            let messages = serde_json::to_value(&context.messages).unwrap();
            let result = messages.as_array().unwrap().last().unwrap();
            assert_eq!(result["isError"], true, "{messages}");
            assert!(result.to_string().contains("backend failure"), "{messages}");
            faux_assistant_message("done", rpi_ai::types::StopReason::Stop)
        }),
    ]);
    let provider = FauxProvider::new(script);
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
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
    let request = output
        .wait_for(|m| m["type"] == "external_tool_request")
        .await;
    assert_eq!(request["arguments"], serde_json::json!({"query":"hello"}));
    let reply = serde_json::json!({"id":3,"type":"external_tool_response","requestId":request["requestId"],"result":{"content":[{"type":"text","text":"remote answer"},{"type":"image","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLttAAAAABJRU5ErkJggg==","mimeType":"image/png"}],"structuredContent":{"preserved":true},"_meta":{"private":"private-metadata"},"toolName":"not_registered","inputSchema":{"type":"object","secret":"private-metadata"}}});
    client
        .write_all(format!("{reply}\n").as_bytes())
        .await
        .unwrap();
    assert_eq!(
        output.wait_for(|m| m["id"] == 3).await["data"]["accepted"],
        true
    );
    let error_request = output
        .wait_for(|m| m["type"] == "external_tool_request" && m["arguments"]["query"] == "error")
        .await;
    let error_reply = serde_json::json!({"id":5,"type":"external_tool_response","requestId":error_request["requestId"],"result":{"content":[{"type":"text","text":"backend failure"}],"isError":true}});
    client
        .write_all(format!("{error_reply}\n").as_bytes())
        .await
        .unwrap();
    assert_eq!(
        output.wait_for(|m| m["id"] == 5).await["data"]["accepted"],
        true
    );
    assert_eq!(
        output.wait_for(|m| m["type"] == "agent_settled").await["outcome"],
        "completed"
    );
    assert_eq!(
        output
            .messages()
            .iter()
            .filter(|m| m["type"] == "external_tool_request")
            .count(),
        2
    );
    // Duplicate responses cannot be accepted after the model has consumed them.
    let mut late = reply;
    late["id"] = serde_json::json!(4);
    client
        .write_all(format!("{late}\n").as_bytes())
        .await
        .unwrap();
    assert_eq!(
        output.wait_for(|m| m["id"] == 4).await["data"]["accepted"],
        false
    );
    client.write_all(b"{\"type\":\"stop\"}\n").await.unwrap();
    assert_eq!(task.await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_abort_during_tool_settles_aborted_before_waiter_retirement() {
    use rpi_ai::providers::faux::{
        faux_assistant_message, FauxBlock, FauxProvider, FauxScript, FauxStep,
    };
    use tokio::io::AsyncWriteExt;
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::factory(|_| {
        faux_assistant_message(
            vec![FauxBlock::tool_call(
                "remote_lookup",
                serde_json::json!({"query":"hold"}),
            )],
            rpi_ai::types::StopReason::ToolUse,
        )
    })]);
    let provider = FauxProvider::new(script);
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
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
    let request = output
        .wait_for(|m| m["type"] == "external_tool_request")
        .await;
    client
        .write_all(b"{\"id\":3,\"type\":\"abort\"}\n")
        .await
        .unwrap();
    assert_eq!(output.wait_for(|m| m["id"] == 3).await["success"], true);
    let settled = output.wait_for(|m| m["type"] == "agent_settled").await;
    assert_eq!(settled["outcome"], "aborted", "{settled}");
    assert_eq!(settled["aborted"], true);
    let late = serde_json::json!({"id":4,"type":"external_tool_response","requestId":request["requestId"],"result":{"content":[]}});
    client
        .write_all(format!("{late}\n").as_bytes())
        .await
        .unwrap();
    assert_eq!(
        output.wait_for(|m| m["id"] == 4).await["data"]["accepted"],
        false
    );
    client.write_all(b"{\"type\":\"stop\"}\n").await.unwrap();
    assert_eq!(task.await.unwrap(), 0);
}
