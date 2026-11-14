use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_plugin_bound_harness_releases_its_provider_and_tool_keepalives() {
    let Some(path) = std::env::var_os("RPI_TEST_SUBAGENT_LIBRARY") else {
        return;
    };
    let provider = FauxProvider::new(FauxScript::new());
    let weak_provider = Arc::downgrade(&provider);
    {
        let ctx = ToolCallContext::default();
        let (host, cell) = rpi_cli::extensions_actions::HarnessActionHost::new_empty(
            vec![],
            std::env::temp_dir(),
            tokio::runtime::Handle::current(),
            BTreeMap::new(),
            ctx.clone(),
        );
        let bridge = ActionBridge::new(tokio::runtime::Handle::current(), Arc::new(host));
        let extensions = rpi_extensions::load_session_mixed(
            &[],
            &[PathBuf::from(path)],
            Arc::new(rpi_extensions::NullDiagnostics),
            Some(bridge),
        );
        let keepalive = extensions.keepalive();
        let weak_keepalive = Arc::downgrade(&keepalive);
        let tools = extensions
            .snapshot()
            .unwrap()
            .tools()
            .iter()
            .map(|t| {
                HarnessTool::new(Arc::new(
                    PluginToolAdapter::new(t.tool.clone(), t.handle(), keepalive.clone())
                        .with_context(ctx.clone()),
                ))
            })
            .collect();
        let parent = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            tools,
            ..Default::default()
        })
        .await
        .unwrap();
        rpi_cli::extensions_actions::HarnessActionHost::set_harness(
            &cell,
            Arc::new(parent.clone()),
        );
        drop(provider);
        drop(extensions);
        drop(keepalive);
        drop(cell);
        drop(parent);
        assert!(weak_keepalive.upgrade().is_none(), "harness -> adapter -> keepalive -> bridge -> host -> harness must not form an Arc cycle");
    }
    assert!(
        weak_provider.upgrade().is_none(),
        "closed sessions must release providers/auth bindings"
    );
}
