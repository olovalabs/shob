//! Build the independent rpi-subagents DLL, then set RPI_TEST_SUBAGENT_LIBRARY.
//! Exercises the real ABI while all model responses and files remain local.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rpi_agent::{AgentEvent, BeforeToolCallResult, CollectorEmitter};
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep};
use rpi_ai::types::StopReason;
use rpi_extensions::{
    ActionBridge, PluginToolAdapter, RuntimeActionHost, ToolCallContext, UiDialogMailbox,
};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::types::{AgentHarnessOptions, HarnessTool};
use serde_json::json;

#[path = "subagent_packages/cancellation.rs"]
mod cancellation;
#[path = "subagent_packages/lifetime.rs"]
mod lifetime;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_subagent_plugin_inherits_project_tools_model_and_parent_approval_ui() {
    let Some(path) = std::env::var_os("RPI_TEST_SUBAGENT_LIBRARY") else {
        eprintln!("SKIP: build rpi-subagents and set RPI_TEST_SUBAGENT_LIBRARY");
        return;
    };
    let path = PathBuf::from(path);
    assert!(path.is_file());
    let temp = tempfile::tempdir().unwrap();
    let ctx = ToolCallContext::new(temp.path().to_string_lossy());
    ctx.set_session_id("default");
    let (host, cell) = rpi_cli::extensions_actions::HarnessActionHost::new_empty(
        vec![],
        temp.path().into(),
        tokio::runtime::Handle::current(),
        BTreeMap::new(),
        ctx.clone(),
    );
    let host: Arc<dyn RuntimeActionHost> = Arc::new(host);
    let ui = UiDialogMailbox::new();
    ui.attach();
    let bridge =
        ActionBridge::with_ui_dialog(tokio::runtime::Handle::current(), host.clone(), ui.clone());
    let extensions = rpi_extensions::load_session_mixed(
        &[],
        std::slice::from_ref(&path),
        Arc::new(rpi_extensions::NullDiagnostics),
        Some(bridge.clone()),
    );
    let registered = extensions.snapshot().expect("DLL must register");
    assert!(registered
        .tools()
        .iter()
        .any(|tool| tool.tool.name == "delegate_task"));
    let mut tools: Vec<_> = registered
        .tools()
        .iter()
        .map(|tool| {
            HarnessTool::new(Arc::new(
                PluginToolAdapter::new(tool.tool.clone(), tool.handle(), extensions.keepalive())
                    .with_context(ctx.clone()),
            ))
        })
        .collect();
    let env = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(temp.path().into()));
    let tool_ctx = rpi_tools::ExecutionToolContext::new(env.clone(), Some(env));
    tools.push(HarnessTool::new(rpi_tools::create_read_tool(
        &tool_ctx, None,
    )));
    tools.push(HarnessTool::new(rpi_tools::create_write_tool(&tool_ctx)));
    std::fs::write(temp.path().join("project.txt"), "project cwd fixture").unwrap();
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call(
            "delegate_task",
            json!({"task":"read project.txt; try blocked.txt","maxTurns":4}),
        ),
        FauxStep::factory(|context| {
            assert!(
                !context
                    .tools
                    .iter()
                    .any(|tool| tool.name == "delegate_task"),
                "{}",
                serde_json::to_string_pretty(context).unwrap()
            );
            assert!(context
                .system_prompt
                .as_deref()
                .unwrap()
                .contains("project instructions"));
            faux_assistant_message(
                vec![rpi_ai::providers::faux::FauxBlock::tool_call(
                    "read",
                    json!({"path":"project.txt"}),
                )],
                StopReason::ToolUse,
            )
        }),
        FauxStep::factory(|context| {
            assert!(serde_json::to_string(context)
                .unwrap()
                .contains("project cwd fixture"));
            faux_assistant_message(
                vec![rpi_ai::providers::faux::FauxBlock::tool_call(
                    "write",
                    json!({"path":"blocked.txt","content":"must not write"}),
                )],
                StopReason::ToolUse,
            )
        }),
        FauxStep::factory(|context| {
            assert!(serde_json::to_string(context)
                .unwrap()
                .contains("permission denied"));
            faux_assistant_message("child report", StopReason::Stop)
        }),
        FauxStep::factory(|context| {
            let text = serde_json::to_string(context).unwrap();
            assert!(text.contains("rpi-delegation"));
            assert!(text.contains("child report"));
            faux_assistant_message("parent completed", StopReason::Stop)
        }),
    ]);
    let provider = FauxProvider::new(script.clone());
    let hook_ui = ui.clone();
    let before_tool_call: rpi_agent::BeforeToolCall = Arc::new(move |context, signal| {
        let ui = hook_ui.clone();
        let name = context.tool_call.name.clone();
        let call_id = context.tool_call.id.clone();
        Box::pin(async move {
            if name != "write" {
                return None;
            }
            let id = format!("child-permission-{call_id}");
            if let Err(error) = ui.handle(json!({"op":"open","requestId":id,"toolCallId":call_id,
                "ui":{"kind":"confirm","purpose":"question","question":"Allow child write?"}}))
            {
                return Some(BeforeToolCallResult {
                    block: true,
                    reason: Some(error),
                    terminate: false,
                    args: None,
                });
            }
            loop {
                if signal.is_cancelled() {
                    let _ = ui.cancel(&id);
                    return Some(BeforeToolCallResult {
                        block: true,
                        reason: Some("cancelled".into()),
                        terminate: false,
                        args: None,
                    });
                }
                let reply = ui.poll(&id).unwrap();
                if reply["status"] != "pending" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Some(BeforeToolCallResult {
                block: true,
                reason: Some("permission denied by parent UI".into()),
                terminate: false,
                args: None,
            })
        })
    });
    let (emitter, events) = CollectorEmitter::new();
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider.clone()],
        tools,
        system_prompt: Some("project instructions".into()),
        before_tool_call: Some(before_tool_call),
        agent_emitter: Some(Arc::new(emitter)),
        ..Default::default()
    })
    .await
    .unwrap();
    rpi_cli::extensions_actions::HarnessActionHost::set_harness(&cell, Arc::new(parent.clone()));
    let answer_ui = ui.clone();
    let answer = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(request) = answer_ui.take_pending() {
                    assert_eq!(request.ui["question"], "Allow child write?");
                    answer_ui
                        .respond(&request.request_id, json!(false))
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("child approval must appear in parent's UI");
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        parent.prompt_text("delegate", vec![]),
    )
    .await
    .unwrap()
    .unwrap();
    answer.await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert!(!temp.path().join("blocked.txt").exists());
    assert!(events.lock().unwrap().iter().any(|event|matches!(event,AgentEvent::ToolExecutionUpdate{tool_name,..} if tool_name=="delegate_task")));
    let context = host.get_execution_context(json!({})).await.unwrap();
    assert_eq!(context["model"]["id"], provider.default_model().id);
    assert_eq!(context["cwd"], temp.path().to_string_lossy().as_ref());
    assert!(context.get("apiKey").is_none());
    assert!(context["model"].get("headers").is_none());
    // The same inherited approval hook must fail closed without a UI consumer.
    ui.detach();
    script.set_responses(vec![
        FauxStep::tool_call("delegate_task", json!({"task":"attempt blocked.txt"})),
        FauxStep::tool_call(
            "write",
            json!({"path":"blocked.txt","content":"must not write"}),
        ),
        FauxStep::factory(|context| {
            assert!(serde_json::to_string(context)
                .unwrap()
                .contains("requires an interactive UI"));
            faux_assistant_message("headless denial observed", StopReason::Stop)
        }),
        FauxStep::text("done"),
    ]);
    tokio::time::timeout(
        Duration::from_secs(10),
        parent.prompt_text("delegate without UI", vec![]),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!temp.path().join("blocked.txt").exists());
    // The same DLL is also available under an alias. A child must not be able
    // to delegate through that alias, even though the native name is filtered.
    let registered_alias = extensions.snapshot().unwrap();
    let alias = registered_alias
        .tools()
        .iter()
        .find(|tool| tool.tool.name == "delegate_task")
        .unwrap();
    let mut alias_schema = alias.tool.clone();
    alias_schema.name = "research_alias".into();
    let mut tools = parent.get_tools().await.unwrap();
    tools.push(HarnessTool::new(Arc::new(
        PluginToolAdapter::new(alias_schema, alias.handle(), extensions.keepalive())
            .with_context(ctx.clone()),
    )));
    parent.set_tools(tools, None).await.unwrap();
    script.set_responses(vec![
        FauxStep::tool_call("delegate_task", json!({"task":"try nested delegation"})),
        FauxStep::tool_call(
            "research_alias",
            json!({"task":"must not spawn a grandchild"}),
        ),
        FauxStep::factory(|context| {
            assert!(serde_json::to_string(context)
                .unwrap()
                .contains("recursive child agents are not allowed"));
            faux_assistant_message("nested delegation blocked", StopReason::Stop)
        }),
        FauxStep::factory(|context| {
            assert!(serde_json::to_string(context)
                .unwrap()
                .contains("nested delegation blocked"));
            faux_assistant_message("parent observed nested denial", StopReason::Stop)
        }),
    ]);
    tokio::time::timeout(
        Duration::from_secs(10),
        parent.prompt_text("delegate via alias", vec![]),
    )
    .await
    .unwrap()
    .unwrap();
    let mut tools = parent.get_tools().await.unwrap();
    tools.retain(|tool| tool.tool.schema().name != "research_alias");
    parent.set_tools(tools, None).await.unwrap();

    // A second registration must not redirect the existing session. Then drop
    // the candidate exactly as a provider-resolution reload rollback does.
    let (other_host, _) = rpi_cli::extensions_actions::HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        tokio::runtime::Handle::current(),
        BTreeMap::new(),
        ToolCallContext::default(),
    );
    let mut candidate_bridge = Some(ActionBridge::new(
        tokio::runtime::Handle::current(),
        Arc::new(other_host),
    ));
    let mut candidate = Some(rpi_extensions::load_session_mixed(
        &[],
        std::slice::from_ref(&path),
        Arc::new(rpi_extensions::NullDiagnostics),
        candidate_bridge.clone(),
    ));
    assert!(candidate.as_ref().unwrap().snapshot().is_some());
    for rollback in [false, true] {
        if rollback {
            drop(candidate.take());
            drop(candidate_bridge.take());
        }
        script.set_responses(vec![
            FauxStep::tool_call("delegate_task", json!({"task":"original session binding"})),
            FauxStep::text("original child binding"),
            FauxStep::factory(|context| {
                assert!(serde_json::to_string(context)
                    .unwrap()
                    .contains("original child binding"));
                faux_assistant_message("original parent binding", StopReason::Stop)
            }),
        ]);
        tokio::time::timeout(
            Duration::from_secs(10),
            parent.prompt_text("delegate across registration/rollback", vec![]),
        )
        .await
        .unwrap()
        .unwrap();
    }

    // Reload the same DLL with a fresh plugin bridge; no OnceLock may keep the
    // old, now-invalid user_data pointer for subsequent delegations.
    let fresh_bridge =
        ActionBridge::with_ui_dialog(tokio::runtime::Handle::current(), host.clone(), ui.clone());
    let reloaded = rpi_extensions::load_session_mixed(
        &[],
        &[path],
        Arc::new(rpi_extensions::NullDiagnostics),
        Some(fresh_bridge.clone()),
    );
    let mut tools = parent.get_tools().await.unwrap();
    tools.retain(|tool| tool.tool.schema().name != "delegate_task");
    for tool in reloaded.snapshot().unwrap().tools() {
        tools.push(HarnessTool::new(Arc::new(
            PluginToolAdapter::new(tool.tool.clone(), tool.handle(), reloaded.keepalive())
                .with_context(ctx.clone()),
        )));
    }
    parent.set_tools(tools, None).await.unwrap();
    bridge.invalidate();
    script.set_responses(vec![
        FauxStep::tool_call("delegate_task", json!({"task":"after reload"})),
        FauxStep::text("reloaded child"),
        FauxStep::factory(|context| {
            assert!(serde_json::to_string(context)
                .unwrap()
                .contains("reloaded child"));
            faux_assistant_message("reloaded parent", StopReason::Stop)
        }),
    ]);
    tokio::time::timeout(
        Duration::from_secs(10),
        parent.prompt_text("delegate after reload", vec![]),
    )
    .await
    .unwrap()
    .unwrap();
    cancellation::repeated_cancel_does_not_exhaust_handles(&parent, &script).await;
    parent.close().await.unwrap();
    fresh_bridge.invalidate();
}
