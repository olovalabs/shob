//! Production dispatcher fixtures: real JSONL reader/writer, harness and tools.
use super::tests::{rpc_test_harness, start_test_rpc};
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

async fn send(client: &mut tokio::io::DuplexStream, value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    client.write_all(&bytes).await.unwrap();
}
fn png_block() -> Value {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[4, 8, 16, 255])
            .unwrap();
    }
    json!({"type":"image", "mimeType":"image/png", "data":STANDARD.encode(bytes)})
}
async fn stop(client: &mut tokio::io::DuplexStream, task: tokio::task::JoinHandle<i32>) {
    send(client, json!({"type":"stop","id":"stop"})).await;
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_prompt_pixels_and_model_are_checked_before_acceptance_or_persistence() {
    let mut model = FauxProvider::new(FauxScript::new()).default_model().clone();
    model.input = vec![rpi_ai::types::InputModality::Text];
    let provider =
        FauxProvider::with_models(FauxScript::new().with_text("unused"), vec![model.clone()]);
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model,
        models: vec![provider.clone()],
        ..Default::default()
    })
    .await
    .unwrap();
    let retained = harness.clone();
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
    for (id, image, expected) in [
        (
            1,
            json!({"type":"image","mimeType":"image/png","data":"AAAA"}),
            "contentBlocks[0]",
        ),
        (2, png_block(), "does not support image"),
    ] {
        send(
            &mut client,
            json!({"type":"prompt","id":id,"contentBlocks":[image]}),
        )
        .await;
        let response = output.wait_for(|m| m["id"] == id).await;
        assert_eq!(response["success"], false);
        assert!(
            response["error"].as_str().unwrap().contains(expected),
            "{response}"
        );
    }
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(retained
        .session()
        .storage()
        .find_entries(&EntryQuery::default())
        .await
        .unwrap()
        .is_empty());
    assert!(!output
        .messages()
        .iter()
        .any(|m| m["type"] == "agent_settled"));
    stop(&mut client, task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_ordered_blocks_reach_model_and_native_history_unchanged() {
    let blocks = vec![
        png_block(),
        json!({"type":"text","text":"middle"}),
        png_block(),
    ];
    let expected: Vec<Content> = blocks
        .iter()
        .cloned()
        .map(|b| serde_json::from_value(b).unwrap())
        .collect();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let seen = captured.clone();
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::factory(move |ctx| {
        *seen.lock().unwrap() = ctx.messages.clone();
        faux_assistant_message("done", StopReason::Stop)
    })]);
    let mut model = FauxProvider::new(FauxScript::new()).default_model().clone();
    model.input.push(rpi_ai::types::InputModality::Image);
    let provider = FauxProvider::with_models(script, vec![model.clone()]);
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model,
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
    send(
        &mut client,
        json!({"type":"prompt","id":1,"contentBlocks":blocks}),
    )
    .await;
    assert_eq!(
        output.wait_for(|m| m["id"] == 1).await["data"]["accepted"],
        true
    );
    output.wait_for(|m| m["type"] == "agent_settled").await;
    let messages = captured.lock().unwrap().clone();
    assert!(messages
        .iter()
        .any(|m| matches!(m, rpi_ai::types::Message::User(u)
        if u.content == rpi_ai::types::UserContent::Blocks(expected.clone()))));
    send(&mut client, json!({"type":"get_messages","id":2})).await;
    let history = output.wait_for(|m| m["id"] == 2).await;
    assert!(history.to_string().contains("middle"));
    assert!(history
        .to_string()
        .contains(blocks[0]["data"].as_str().unwrap()));
    stop(&mut client, task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_prompt_alias_precedence_and_bad_block_shape_never_fall_back() {
    let (harness, provider) = rpc_test_harness("done", 0.0).await;
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
    for (id, payload) in [
        (1, json!({"message":null,"content":"fallback"})),
        (2, json!({"message":"fallback","contentBlocks":null})),
        (3, json!({"contentBlocks":[{"type":"text","text":42}]})),
    ] {
        let mut command = payload;
        command["type"] = json!("prompt");
        command["id"] = json!(id);
        send(&mut client, command).await;
        assert_eq!(output.wait_for(|m| m["id"] == id).await["success"], false);
    }
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    send(
        &mut client,
        json!({"type":"prompt","id":4,"message":"first","content":"second","prompt":"third"}),
    )
    .await;
    assert_eq!(output.wait_for(|m| m["id"] == 4).await["success"], true);
    output.wait_for(|m| m["type"] == "agent_settled").await;
    stop(&mut client, task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_file_io_dispatch_reads_unsaved_client_text_and_rejects_active_configuration() {
    let router = Arc::new(rpi_tools::file_io::FileIORouter::new());
    let env = Arc::new(rpi_tools::InMemoryExecutionEnv::new());
    let context =
        rpi_tools::ExecutionToolContext::new(env.clone(), Some(env)).with_file_io(router.clone());
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("read", json!({"path":"/workspace/unsaved.txt"}))
            .with_text("done"),
    );
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        tools: vec![rpi_harness::types::HarnessTool::new(
            rpi_tools::create_read_tool(&context, None),
        )],
        ..Default::default()
    })
    .await
    .unwrap();
    crate::session::register_file_io_router(&harness, &router);
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
    let ready = output.wait_for(|m| m["type"] == "ready").await;
    assert_eq!(ready["fileIO"], crate::rpc::file_io::capabilities());
    assert_eq!(
        ready["promptContent"],
        crate::rpc::prompt_content::prompt_capabilities()
    );
    assert_eq!(ready["semantics"]["messageIds"], "sessionEntryId-v1");
    send(
        &mut client,
        json!({"type":"configure_file_io","id":1,"readText":true,"writeText":false}),
    )
    .await;
    assert_eq!(output.wait_for(|m| m["id"] == 1).await["success"], true);
    send(
        &mut client,
        json!({"type":"prompt","id":2,"message":"read unsaved buffer"}),
    )
    .await;
    let request = output.wait_for(|m| m["type"] == "file_io_request").await;
    assert_eq!(request["path"], "/workspace/unsaved.txt");
    send(
        &mut client,
        json!({"type":"configure_file_io","id":3,"readText":false,"writeText":false}),
    )
    .await;
    assert_eq!(output.wait_for(|m| m["id"] == 3).await["success"], false);
    send(
        &mut client,
        json!({"type":"set_model","id":4,"modelId":"other"}),
    )
    .await;
    assert!(output.wait_for(|m| m["id"] == 4).await["error"]
        .as_str()
        .unwrap()
        .contains("active"));
    send(&mut client, json!({"type":"file_io_response","id":"read-reply","requestId":request["requestId"],"result":{"content":"unsaved client buffer\n"}})).await;
    let reply = output.wait_for(|m| m["id"] == "read-reply").await;
    assert_eq!(reply["command"], "file_io_response");
    assert_eq!(reply["data"]["accepted"], true);
    output
        .wait_for(|m| {
            m["type"] == "file_io_request_closed" && m["requestId"] == request["requestId"]
        })
        .await;
    output.wait_for(|m| m["type"] == "agent_settled").await;
    send(&mut client, json!({"type":"get_messages","id":5})).await;
    assert!(output
        .wait_for(|m| m["id"] == 5)
        .await
        .to_string()
        .contains("unsaved client buffer"));
    send(
        &mut client,
        json!({"type":"configure_file_io","id":6,"readText":false,"writeText":false}),
    )
    .await;
    assert_eq!(output.wait_for(|m| m["id"] == 6).await["success"], true);
    stop(&mut client, task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_file_io_write_and_edit_use_the_same_client_backend() {
    let router = Arc::new(rpi_tools::file_io::FileIORouter::new());
    let env = Arc::new(rpi_tools::InMemoryExecutionEnv::new());
    let context =
        rpi_tools::ExecutionToolContext::new(env.clone(), Some(env)).with_file_io(router.clone());
    let provider = FauxProvider::new(FauxScript::new()
        .with_tool_call("write", json!({"path":"/workspace/buffer.txt","content":"original"}))
        .with_tool_call("edit", json!({"path":"/workspace/buffer.txt","edits":[{"oldText":"original","newText":"edited"}]}))
        .with_text("done"));
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        tools: vec![
            rpi_harness::types::HarnessTool::new(rpi_tools::create_write_tool(&context)),
            rpi_harness::types::HarnessTool::new(rpi_tools::create_edit_tool(&context)),
        ],
        ..Default::default()
    })
    .await
    .unwrap();
    crate::session::register_file_io_router(&harness, &router);
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
    send(
        &mut client,
        json!({"type":"configure_file_io","id":1,"readText":true,"writeText":true}),
    )
    .await;
    assert_eq!(output.wait_for(|m| m["id"] == 1).await["success"], true);
    send(
        &mut client,
        json!({"type":"prompt","id":2,"message":"write then edit"}),
    )
    .await;
    let mut previous = Value::Null;
    for (operation, content) in [
        ("write_text_file", Some("original")),
        ("read_text_file", None),
        ("write_text_file", Some("edited")),
    ] {
        let request = output
            .wait_for(|m| {
                m["type"] == "file_io_request"
                    && m["requestId"] != previous
                    && !output.messages().iter().any(|closed| {
                        closed["type"] == "file_io_request_closed"
                            && closed["requestId"] == m["requestId"]
                    })
            })
            .await;
        assert_eq!(request["operation"], operation);
        assert_eq!(request["path"], "/workspace/buffer.txt");
        if let Some(content) = content {
            assert_eq!(request["content"], content);
        }
        let result = if operation == "read_text_file" {
            json!({"content":"original"})
        } else {
            json!({})
        };
        let reply_id = format!("reply-{}", request["requestId"].as_str().unwrap());
        send(
            &mut client,
            json!({"type":"file_io_response","id":reply_id,"requestId":request["requestId"],"result":result}),
        )
        .await;
        let reply = output.wait_for(|m| m["id"] == reply_id).await;
        assert_eq!(reply["command"], "file_io_response");
        assert_eq!(reply["data"]["accepted"], true);
        output
            .wait_for(|m| {
                m["type"] == "file_io_request_closed" && m["requestId"] == request["requestId"]
            })
            .await;
        previous = request["requestId"].clone();
    }
    assert_eq!(
        output.wait_for(|m| m["type"] == "agent_settled").await["outcome"],
        "completed"
    );
    stop(&mut client, task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_validation_is_responsive_and_root_abort_retains_worker_cleanup() {
    use crate::rpc::prompt_content::validation_test_hook::{Gate, ReleaseOnDrop};
    use rpi_tools::file_io::{FileIOErrorCode, FileIOOperation, FileIOReadRange};
    use std::time::Duration;
    for abort_root in [false, true] {
        let id = if abort_root {
            "validation-root-abort"
        } else {
            "validation-command-abort"
        };
        let gate = Gate::register(id);
        let _release = ReleaseOnDrop(gate.clone());
        let (harness, provider) = rpc_test_harness("unused", 0.0).await;
        let retained = harness.clone();
        let router = Arc::new(rpi_tools::file_io::FileIORouter::new());
        crate::session::register_file_io_router(&harness, &router);
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        send(&mut client, json!({"type":"configure_file_io","id":"initial-config","readText":true,"writeText":false})).await;
        assert_eq!(
            output.wait_for(|m| m["id"] == "initial-config").await["success"],
            true
        );
        // This snapshot must remain client-authoritative after owner drop.
        let backend = router.select(FileIOOperation::Read).await.unwrap().unwrap();
        send(
            &mut client,
            json!({"type":"prompt","id":id,"contentBlocks":[png_block()]}),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(3), gate.entered.cancelled())
            .await
            .unwrap();
        send(&mut client, json!({"type":"configure_file_io","id":"active-config","readText":false,"writeText":false})).await;
        assert!(
            output.wait_for(|m| m["id"] == "active-config").await["error"]
                .as_str()
                .unwrap()
                .contains("active")
        );
        send(
            &mut client,
            json!({"type":"set_model","id":"active-model","modelId":"other"}),
        )
        .await;
        assert!(
            output.wait_for(|m| m["id"] == "active-model").await["error"]
                .as_str()
                .unwrap()
                .contains("active")
        );
        send(&mut client, json!({"type":"ping","id":"during-validation"})).await;
        assert_eq!(
            output.wait_for(|m| m["id"] == "during-validation").await["success"],
            true
        );
        assert!(!gate.worker_finished.is_cancelled());
        if abort_root {
            task.abort();
            assert!(tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap_err()
                .is_cancelled());
            assert!(!ui.is_attached());
            // Cancelling the root must not drop/abort the run's worker owner.
            assert!(!gate.run_finished.is_cancelled());
            assert!(!gate.worker_finished.is_cancelled());
            gate.release();
            tokio::time::timeout(Duration::from_secs(3), gate.run_finished.cancelled())
                .await
                .unwrap();
        } else {
            send(&mut client, json!({"type":"abort","id":"abort-validation"})).await;
            assert_eq!(
                output.wait_for(|m| m["id"] == "abort-validation").await["success"],
                true
            );
            assert!(!gate.run_finished.is_cancelled());
            gate.release();
            tokio::time::timeout(Duration::from_secs(3), gate.run_finished.cancelled())
                .await
                .unwrap();
            let rejection = output.wait_for(|m| m["id"] == id).await;
            assert_eq!(rejection["success"], false);
            assert!(rejection["error"].as_str().unwrap().contains("aborted"));
            stop(&mut client, task).await;
        }
        assert!(gate.worker_finished.is_cancelled());
        assert_eq!(
            provider
                .state()
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(retained
            .session()
            .storage()
            .find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .is_empty());
        assert!(retained
            .lane("main")
            .snapshot()
            .await
            .unwrap()
            .active
            .is_none());
        assert!(!output
            .messages()
            .iter()
            .any(|m| m["type"] == "agent_settled"
                || (m["id"] == id && m["data"]["accepted"] == true)));
        assert_eq!(
            backend
                .read_text(
                    "/workspace/no-host-fallback.txt",
                    FileIOReadRange::Complete,
                    &tokio_util::sync::CancellationToken::new()
                )
                .await
                .unwrap_err()
                .code,
            FileIOErrorCode::Disconnected
        );
        // Same router still selects client I/O, never a host fallback.
        assert!(router
            .select(FileIOOperation::Read)
            .await
            .unwrap()
            .is_some());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_correlated_file_io_reply_preserves_unknown_field_rejection() {
    let router = Arc::new(rpi_tools::file_io::FileIORouter::new());
    let env = Arc::new(rpi_tools::InMemoryExecutionEnv::new());
    let context =
        rpi_tools::ExecutionToolContext::new(env.clone(), Some(env)).with_file_io(router.clone());
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("read", json!({"path":"/workspace/buffer.txt"}))
            .with_text("done"),
    );
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        tools: vec![rpi_harness::types::HarnessTool::new(
            rpi_tools::create_read_tool(&context, None),
        )],
        ..Default::default()
    })
    .await
    .unwrap();
    crate::session::register_file_io_router(&harness, &router);
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
    send(
        &mut client,
        json!({"type":"configure_file_io","id":1,"readText":true,"writeText":false}),
    )
    .await;
    assert_eq!(output.wait_for(|m| m["id"] == 1).await["success"], true);
    send(
        &mut client,
        json!({"type":"prompt","id":2,"message":"read buffer"}),
    )
    .await;
    let request = output.wait_for(|m| m["type"] == "file_io_request").await;
    send(
        &mut client,
        json!({"type":"file_io_response","id":"bad-reply","requestId":request["requestId"],
        "result":{"content":"MUST NOT BE PERSISTED"},"unknown":true}),
    )
    .await;
    // A known malformed reply retires its waiter with InvalidResponse; accepted
    // means correlated/consumed, not successful file I/O.
    assert_eq!(
        output.wait_for(|m| m["id"] == "bad-reply").await["data"]["accepted"],
        true
    );
    output.wait_for(|m| m["type"] == "agent_settled").await;
    send(&mut client, json!({"type":"get_messages","id":3})).await;
    let history = output.wait_for(|m| m["id"] == 3).await.to_string();
    assert!(
        history.contains("invalid client file I/O response"),
        "{history}"
    );
    assert!(!history.contains("MUST NOT BE PERSISTED"));
    stop(&mut client, task).await;
}

#[tokio::test]
async fn production_parallel_admission_and_abort_before_validation_are_atomic() {
    let (harness, provider) = rpc_test_harness("unused", 0.0).await;
    let (mut client, output, task) =
        start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
    // One buffered read on a current-thread runtime: the run task cannot
    // validate/persist until the dispatcher yields after these control frames.
    client.write_all(b"{\"type\":\"prompt\",\"id\":1,\"contentBlocks\":[{\"type\":\"text\",\"text\":\"first\"}]}\n{\"type\":\"prompt\",\"id\":2,\"message\":\"second\"}\n{\"type\":\"abort\",\"id\":3}\n").await.unwrap();
    assert_eq!(output.wait_for(|m| m["id"] == 2).await["success"], false);
    assert_eq!(output.wait_for(|m| m["id"] == 3).await["success"], true);
    let rejected = output.wait_for(|m| m["id"] == 1).await;
    assert_eq!(rejected["success"], false);
    assert!(rejected["error"].as_str().unwrap().contains("aborted"));
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(!output
        .messages()
        .iter()
        .any(|m| m["type"] == "agent_settled"));
    stop(&mut client, task).await;
}
