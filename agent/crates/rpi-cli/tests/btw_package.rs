//! Real command ABI test. Build rpi-btw, set RPI_TEST_BTW_LIBRARY, run this test.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep};
use rpi_ai::types::StopReason;
use rpi_extensions::{ActionBridge, RuntimeActionHost, ToolCallContext};
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::types::AgentHarnessOptions;
use rpi_plugin_sdk::{StbString, StbStringRef};
use serde_json::{json, Value};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_btw_command_is_owner_bound_and_never_writes_parent_history() {
    let Some(path) = std::env::var_os("RPI_TEST_BTW_LIBRARY") else {
        eprintln!("SKIP: build rpi-btw and set RPI_TEST_BTW_LIBRARY");
        return;
    };
    let path = PathBuf::from(path);
    assert!(path.is_file());
    let temp = tempfile::tempdir().unwrap();
    let (host, cell) = rpi_cli::extensions_actions::HarnessActionHost::new_empty(
        vec![],
        temp.path().into(),
        tokio::runtime::Handle::current(),
        BTreeMap::new(),
        ToolCallContext::new(temp.path().to_string_lossy()),
    );
    let host: Arc<dyn RuntimeActionHost> = Arc::new(host);
    let bridge = ActionBridge::new(tokio::runtime::Handle::current(), host.clone());
    let extensions = rpi_extensions::load_session_mixed(
        &[],
        std::slice::from_ref(&path),
        Arc::new(rpi_extensions::NullDiagnostics),
        Some(bridge.clone()),
    );
    let snapshot = extensions.snapshot().expect("btw DLL must load");
    assert!(
        snapshot.tools().is_empty(),
        "btw must register no model tools"
    );
    let command = snapshot
        .commands()
        .iter()
        .find(|command| command.name == "btw")
        .unwrap()
        .clone();
    let invoke = |args: &str| -> Value {
        let encoded = json!({"command":"btw","args":args}).to_string();
        let mut out = StbString::empty();
        assert_eq!(
            (command.handler)(
                StbStringRef::from_str(&encoded),
                &mut out,
                command.user_data
            ),
            0
        );
        // SAFETY: the command transfers this live string to the host, exactly
        // as the interactive command dispatcher reclaims its callback output.
        let output = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(
                out,
                Some(rpi_extensions::host_free_string),
            )
        };
        serde_json::from_str(&output.to_string_lossy()).unwrap()
    };
    assert!(invoke("help")["text"]
        .as_str()
        .unwrap()
        .contains("/btw cancel"));
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::text("# Side answer\n\n**side answer**"),
        FauxStep::factory(|ctx| {
            assert!(ctx.tools.is_empty());
            let text = serde_json::to_string(&ctx.messages).unwrap();
            assert!(text.contains("side answer"));
            assert!(text.contains("follow up"));
            faux_assistant_message("follow-up answer", StopReason::Stop)
        }),
        FauxStep::text("new topic answer"),
        FauxStep::text("candidate binding answer"),
    ]);
    let provider = FauxProvider::new(script);
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    cell.set(parent.downgrade()).ok().unwrap();
    assert!(invoke("")["text"].as_str().unwrap().contains("opened"));
    assert_eq!(host.side_question_views().len(), 1);
    assert!(host.side_question_views()[0]["turns"]
        .as_array()
        .unwrap()
        .is_empty());
    let started = invoke("顺便问一下");
    assert_eq!(started["markdown"], true);
    assert!(
        started["text"].as_str().unwrap().contains("BTW started"),
        "{started}"
    );
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let value = invoke("status");
            if value["text"].as_str().unwrap().contains("BTW · done") {
                assert!(value["text"].as_str().unwrap().contains("side answer"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(parent.get_tip_id().await.unwrap().is_none());
    assert_eq!(host.side_question_views()[0]["visible"], true);
    assert!(
        bridge.extension_status_mailbox().panels().is_empty(),
        "native chat must not publish a text preview panel"
    );
    assert!(invoke("hide")["text"].as_str().unwrap().contains("hidden"));
    assert_eq!(host.side_question_views()[0]["visible"], false);
    invoke("show");
    assert_eq!(host.side_question_views()[0]["visible"], true);
    invoke("follow up");
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let value = invoke("status");
            if value["text"].as_str().unwrap().contains("follow-up answer") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        host.side_question_views()[0]["turns"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let old_thread = host.side_question_views()[0]["threadId"]
        .as_str()
        .unwrap()
        .to_owned();
    invoke("new independent topic");
    assert_ne!(host.side_question_views()[0]["threadId"], old_thread);
    assert!(parent.get_tip_id().await.unwrap().is_none());
    // A second registration of the same DLL must not change the first
    // command's binding. Drop the candidate without invalidating the old one.
    let candidate_bridge = ActionBridge::new(tokio::runtime::Handle::current(), host.clone());
    let candidate = rpi_extensions::load_session_mixed(
        &[],
        std::slice::from_ref(&path),
        Arc::new(rpi_extensions::NullDiagnostics),
        Some(candidate_bridge.clone()),
    );
    assert!(candidate.snapshot().is_some());
    drop(candidate);
    drop(candidate_bridge);
    // Wait for the previous request to finish so this one is started, not queued.
    tokio::time::timeout(Duration::from_secs(4), async {
        while !invoke("status")["text"]
            .as_str()
            .unwrap()
            .contains("new topic answer")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let feedback = invoke("顺便问一下");
    assert!(
        feedback["text"].as_str().unwrap().contains("BTW started")
            || feedback["text"].as_str().unwrap().contains("queued")
    );
    invoke("cancel");
    invoke("clear");
    assert!(host.side_question_views().is_empty());
    bridge.invalidate();
    assert!(host.side_question_views().is_empty());
    assert!(invoke("another question")["text"]
        .as_str()
        .unwrap()
        .contains("stale"));
    parent.close().await.unwrap();
}
