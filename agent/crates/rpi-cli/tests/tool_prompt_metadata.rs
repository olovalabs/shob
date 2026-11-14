//! Build `plugin-stub` before running this test to exercise the real native ABI.
use rpi_cli::args::Args;
use rpi_cli::session::{build, reload_extension_resources};

#[test]
fn explicit_tool_prompts_follow_startup_reload_and_tool_filtering() {
    const CHILD: &str = "RPI_TOOL_PROMPT_METADATA_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let executable = std::env::current_exe().unwrap();
        let profile = executable.parent().unwrap().parent().unwrap();
        let library = profile.join(if cfg!(windows) {
            "plugin_stub.dll"
        } else if cfg!(target_os = "macos") {
            "libplugin_stub.dylib"
        } else {
            "libplugin_stub.so"
        });
        if !library.is_file() {
            eprintln!("SKIP: build plugin-stub before running tool_prompt_metadata");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(executable)
            .args([
                "--exact",
                "explicit_tool_prompts_follow_startup_reload_and_tool_filtering",
                "--nocapture",
            ])
            .env(CHILD, &library)
            .env("RPI_CODING_AGENT_DIR", temp.path())
            .env_remove("RPI_EXTENSIONS_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let cwd = tempfile::tempdir().unwrap();
            let resolved = rpi_cli::provider::resolve(
                Some("anthropic"),
                Some(rpi_cli::provider::DEFAULT_MODEL_ID),
                None,
                Some("local-test-key"),
                None,
            )
            .unwrap();
            let args = Args {
                no_session: true,
                no_context_files: true,
                no_skills: true,
                no_prompt_templates: true,
                dev_local_only: true,
                tools: Some(vec!["echo".into()]),
                extension: vec![std::env::var_os(CHILD).unwrap().into()],
                ..Default::default()
            };
            let (harness, _events, mut context) =
                build(&resolved, &args, cwd.path(), true).await.unwrap();
            let snippet = "- echo  — Echo the supplied text";
            let rule = "Use echo only when an echo is requested";
            let prompt = harness.get_system_prompt().await.unwrap().unwrap();
            assert!(prompt.contains(snippet), "{prompt}");
            assert_eq!(prompt.matches(rule).count(), 1);
            let tools = harness.get_tools().await.unwrap();
            let echo = tools
                .iter()
                .find(|tool| tool.tool.schema().name == "echo")
                .unwrap();
            assert!(echo.tool.schema().description.contains("Echoes back"));
            drop(tools);

            // Rebuild from the current extension snapshot, not cached prompt metadata.
            let outcome = reload_extension_resources(&harness, &context).await;
            assert!(!outcome.had_errors, "{}", outcome.summary);
            let prompt = harness.get_system_prompt().await.unwrap().unwrap();
            assert!(prompt.contains(snippet));
            assert_eq!(prompt.matches(rule).count(), 1);

            // A loaded tool's contributions disappear when it is inactive.
            context.args.tools = Some(vec!["read".into()]);
            reload_extension_resources(&harness, &context).await;
            let prompt = harness.get_system_prompt().await.unwrap().unwrap();
            assert!(!prompt.contains(snippet));
            assert!(!prompt.contains(rule));
            assert!(prompt.contains("- read  — Read file contents"));

            // An explicit denylist suppresses the same contributions.
            context.args.tools = Some(vec!["echo".into()]);
            context.args.exclude_tools = Some(vec!["echo".into()]);
            reload_extension_resources(&harness, &context).await;
            let prompt = harness.get_system_prompt().await.unwrap().unwrap();
            assert!(!prompt.contains(snippet));
            assert!(!prompt.contains(rule));
            assert!(!harness
                .get_tools()
                .await
                .unwrap()
                .iter()
                .any(|tool| tool.tool.schema().name == "echo"));

            // Re-enabling brings the contributions back, without duplication.
            context.args.exclude_tools = None;
            context.args.tools = Some(vec!["echo".into()]);
            reload_extension_resources(&harness, &context).await;
            let prompt = harness.get_system_prompt().await.unwrap().unwrap();
            assert!(prompt.contains(snippet));
            assert_eq!(prompt.matches(rule).count(), 1);

            // An explicit system prompt remains an exact replacement.
            context.args.system_prompt = Some("CUSTOM PROMPT".into());
            reload_extension_resources(&harness, &context).await;
            assert_eq!(
                harness.get_system_prompt().await.unwrap().as_deref(),
                Some("CUSTOM PROMPT")
            );

            // Unloading a tool must not leave prompt metadata behind.
            context.args.system_prompt = None;
            context.args.extension.clear();
            reload_extension_resources(&harness, &context).await;
            let prompt = harness.get_system_prompt().await.unwrap().unwrap();
            assert!(!prompt.contains(snippet));
            assert!(!prompt.contains(rule));
        });
}
