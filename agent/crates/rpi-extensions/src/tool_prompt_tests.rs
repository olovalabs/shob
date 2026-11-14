use super::*;
use rpi_plugin_sdk::{StepHandle, StepResult, ToolPartialCb};

extern "C" fn execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    let _params = unsafe { rpi_plugin_sdk::callbacks::OwnedString::from_raw(params, free) };
    std::ptr::null_mut()
}
extern "C" fn poll(_: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    StepResult::err(StbString::from_string("not executed".into()))
}
extern "C" fn cleanup(_: StepHandle) {}

fn registry() -> ExtensionRegistry {
    let mut registry = ExtensionRegistry::new();
    registry.register_tool(
        rpi_ai::types::Tool {
            name: "example".into(),
            description: "Provider-facing description, not a prompt snippet".into(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({"type":"object"})),
            constrained_sampling: None,
        },
        PluginToolHandle {
            execute_fn: execute,
            poll_fn: poll,
            cancel_fn: cleanup,
            destroy_fn: cleanup,
            plugin_free_string: host_free_string,
            set_waker_fn: None,
            runtime_handle: None,
        },
    );
    registry
}

#[test]
fn tool_prompt_registration_validates_and_copies_metadata_without_changing_schema() {
    let host = HostApi::new("example-plugin", registry(), Arc::new(NullDiagnostics));
    let register = host.build_vtable().register_tool_prompt.unwrap();
    let name = StbStringRef::from_str("example");
    assert_eq!(register(name, StbStringRef::from_str("{}")), -1);
    let _registration = enter_current_api(&host);
    for json in [
        "null",
        "[]",
        "invalid",
        r#"{"promptSnippet":5}"#,
        r#"{"promptGuidelines":[false]}"#,
    ] {
        assert_eq!(register(name, StbStringRef::from_str(json)), 2, "{json}");
    }
    assert_eq!(
        register(
            StbStringRef::from_str("foreign-tool"),
            StbStringRef::from_str("{}")
        ),
        1
    );
    let json = String::from(
        r#"{"promptSnippet":"  Explicit summary  ","promptGuidelines":["  A rule  "," "]}"#,
    );
    assert_eq!(register(name, StbStringRef::from_str(&json)), 0);
    drop(json); // The snapshot must own its strings, not borrow plugin memory.
    let snapshot = host.take_registry().unwrap().snapshot();
    let tool = &snapshot.tools()[0];
    assert_eq!(
        tool.prompt.prompt_snippet.as_deref(),
        Some("Explicit summary")
    );
    assert_eq!(tool.prompt.prompt_guidelines, ["A rule"]);
    assert_eq!(
        tool.tool.description,
        "Provider-facing description, not a prompt snippet"
    );
    assert_eq!(register(name, StbStringRef::from_str("{}")), -1);
}

#[test]
fn tool_prompt_metadata_follows_the_winning_tool_and_does_not_leak_between_snapshots() {
    let mut first = registry();
    let old = first.snapshot();
    assert_eq!(old.tools()[0].prompt, ToolPromptMetadata::default());
    assert!(first.register_tool_prompt(
        "example",
        ToolPromptMetadata {
            prompt_snippet: Some("first".into()),
            prompt_guidelines: vec!["First rule".into()],
        }
    ));
    let mut second = registry();
    assert!(second.register_tool_prompt(
        "example",
        ToolPromptMetadata {
            prompt_snippet: Some("losing registration".into()),
            prompt_guidelines: vec!["Losing rule".into()],
        }
    ));
    first.absorb(second);
    let current = first.snapshot();
    assert_eq!(current.tools().len(), 1);
    assert_eq!(
        current.tools()[0].prompt.prompt_snippet.as_deref(),
        Some("first")
    );
    assert_eq!(current.tools()[0].prompt.prompt_guidelines, ["First rule"]);
    assert_eq!(old.tools()[0].prompt, ToolPromptMetadata::default());
    match &current.entries()[0] {
        RegistryEntry::Tool(tool) => assert_eq!(tool.prompt, current.tools()[0].prompt),
        _ => panic!("expected tool entry"),
    }
    assert!(first.register_tool_prompt("example", ToolPromptMetadata::default()));
    assert_eq!(
        first.snapshot().tools()[0].prompt,
        ToolPromptMetadata::default()
    );
    assert_eq!(
        current.tools()[0].prompt.prompt_snippet.as_deref(),
        Some("first")
    );
}
