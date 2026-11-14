use super::*;

struct RestoreSmokeConfig(Option<std::ffi::OsString>);
impl Drop for RestoreSmokeConfig {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
            None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
        }
    }
}

/// A real separately-built plugin crosses the SDK ABI and runtime action bridge.
/// No provider/network request or background goal worker is used.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "set RPI_GOAL_SMOKE_DLL to the matching rpi-goal cdylib"]
async fn native_goal_outbox_smoke() {
    use rpi_harness::agent_harness::AgentLane;
    use rpi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery};
    use rpi_plugin_sdk::EventTag;
    use serde_json::json;
    let dll = std::env::var_os("RPI_GOAL_SMOKE_DLL").expect("RPI_GOAL_SMOKE_DLL is required");
    let temporary = tempfile::tempdir().unwrap();
    let _config = RestoreSmokeConfig(std::env::var_os(crate::config::CONFIG_DIR_ENV));
    let agent = temporary.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
    let args = Args {
        extension: vec![dll.into()],
        dev_local_only: true,
        no_builtin_tools: true,
        no_skills: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("test".into()),
        session_dir: Some(temporary.path().join("sessions")),
        ..Default::default()
    };
    let resolved = crate::provider::resolve(
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("test-key"),
        None,
    )
    .unwrap();
    let (harness, _events, context) = build(&resolved, &args, temporary.path(), false)
        .await
        .unwrap();
    let tool = harness
        .get_tools()
        .await
        .unwrap()
        .into_iter()
        .find(|tool| tool.tool.schema().name == "goal")
        .expect("goal DLL must load")
        .tool;
    let execute = |params| {
        tool.execute(
            "smoke",
            params,
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        )
    };
    let started = execute(json!({"action":"start","title":"Branch A","maxTurns":2}))
        .await
        .unwrap();
    assert_eq!(started.details["goal"]["title"], "Branch A");
    let source = harness.view().get_leaf_id().await.unwrap().unwrap();
    assert!(harness
        .view()
        .find_entries_on_branch(
            &EntryQuery {
                entry_type: Some("message"),
                ..Default::default()
            },
            &BranchBounds::default()
        )
        .await
        .unwrap()
        .is_empty());
    // Neither branch navigation nor session switching dispatches a goal callback.
    harness
        .navigate_tree(None, false, None, None)
        .await
        .unwrap();
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["active"],
        false
    );
    execute(json!({"action":"start","title":"Branch B"}))
        .await
        .unwrap();
    harness
        .navigate_tree(Some(&source), false, None, None)
        .await
        .unwrap();
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["goal"]["title"],
        "Branch A"
    );
    assert!(dispatch_session_event_async(&context, EventTag::TurnEnd)
        .await
        .is_none());
    assert!(dispatch_session_event_async(&context, EventTag::TurnEnd)
        .await
        .is_none());
    let shown = execute(json!({"action":"show"})).await.unwrap();
    assert_eq!(shown.details["goal"]["status"], "paused");
    assert_eq!(shown.details["goal"]["turnsUsed"], 2);
    let entries = harness
        .view()
        .find_entries_on_branch(
            &EntryQuery {
                entry_type: Some("message"),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .unwrap();
    assert_eq!(entries.len(), 2, "initial goal and one budget-pause notice");
    assert!(dispatch_session_event_async(&context, EventTag::TurnEnd)
        .await
        .is_none());
    assert_eq!(
        harness
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    entry_type: Some("message"),
                    ..Default::default()
                },
                &BranchBounds::default()
            )
            .await
            .unwrap()
            .len(),
        2
    );
    let original = harness.session().storage().metadata().id;
    let host = context
        .action_bridge
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone_host();
    host.new_session(json!({})).await.unwrap();
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["active"],
        false
    );
    host.switch_session(json!({"id":original})).await.unwrap();
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["goal"]["title"],
        "Branch A"
    );
    assert!(
        !reload_extension_resources(&harness, &context)
            .await
            .had_errors
    );
    let tool = harness
        .get_tools()
        .await
        .unwrap()
        .into_iter()
        .find(|tool| tool.tool.schema().name == "goal")
        .unwrap()
        .tool;
    assert_eq!(
        tool.execute(
            "reload",
            json!({"action":"show"}),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {})
        )
        .await
        .unwrap()
        .details["goal"]["status"],
        "paused"
    );
}
