use std::collections::BTreeMap;

use rpi_cli::session::{
    default_system_prompt, default_system_prompt_with_tool_prompts,
    default_system_prompt_with_tools, BUILTIN_TOOL_NAMES,
};
use rpi_extensions::ToolPromptMetadata;

#[test]
fn published_two_argument_prompt_api_remains_callable() {
    // Type-check the API as a downstream crate, rather than through a private helper.
    let prompt: fn(&str, bool) -> String = default_system_prompt;
    for replayed in [false, true] {
        let text = prompt("/compat/project", replayed);
        assert!(text.contains("Current working directory: /compat/project"));
        for tool in BUILTIN_TOOL_NAMES {
            assert!(text.contains(&format!("- {tool}")));
        }
        assert_eq!(text.contains("reasoning is carried back"), replayed);
        assert!(!text.contains("todo tool"));
    }
}

#[test]
fn selected_tool_prompt_keeps_restricted_runs_restricted() {
    let text = default_system_prompt_with_tools("/compat/project", true, &["read".to_owned()]);
    assert!(text.contains("- read"));
    for absent in [
        "- bash",
        "- edit",
        "- write",
        "- docs",
        "todo tool",
        "PowerShell cmdlets",
    ] {
        assert!(
            !text.contains(absent),
            "unavailable tool guidance: {absent}"
        );
    }
}

#[test]
fn tool_summary_omits_extensions_without_snippets_and_preserves_order() {
    let tools = [
        "write",
        "subagent",
        "read",
        "plan",
        "dispatch",
        "delegate_task",
    ]
    .map(str::to_owned);
    for replayed in [false, true] {
        let text = default_system_prompt_with_tools("/compat/project", replayed, &tools);
        let summary = text
            .split_once("Available tools:\n")
            .unwrap()
            .1
            .split_once("\n\n")
            .unwrap()
            .0;
        assert_eq!(
            summary,
            "- write — Create or overwrite files\n- read  — Read file contents"
        );
        for absent in ["subagent", "plan", "dispatch", "delegate_task"] {
            assert!(
                !summary.contains(absent),
                "tool without a snippet must not be listed: {absent}"
            );
        }
        assert!(text.contains("you may have access to other custom tools"));
        for absent in ["- bash", "- edit", "- docs", "todo tool"] {
            assert!(
                !text.contains(absent),
                "unavailable tool guidance: {absent}"
            );
        }
    }
}

#[test]
fn tool_summary_has_an_empty_state_after_snippet_filtering() {
    for tools in [
        Vec::new(),
        vec!["subagent".to_owned(), "delegate_task".to_owned()],
    ] {
        for replayed in [false, true] {
            let text = default_system_prompt_with_tools("/compat/project", replayed, &tools);
            assert!(text.contains("Available tools:\n(none)\n\n"));
            assert!(text.contains("you may have access to other custom tools"));
            assert!(!text.contains("- subagent"));
            assert!(!text.contains("- delegate_task"));
            assert!(!text.contains("todo tool"));
        }
    }
}

#[test]
fn active_tool_guidance_does_not_require_a_prompt_snippet() {
    let metadata = BTreeMap::from([(
        "todo".to_owned(),
        ToolPromptMetadata {
            prompt_snippet: None,
            prompt_guidelines: vec!["Track work with the todo tool".to_owned()],
        },
    )]);
    for replayed in [false, true] {
        let text = default_system_prompt_with_tool_prompts(
            "/compat/project",
            replayed,
            &["todo".to_owned()],
            &metadata,
        );
        assert!(text.contains("Available tools:\n(none)\n\n"));
        assert!(!text.contains("- todo"));
        assert_eq!(text.matches("todo tool").count(), 1);
        assert_eq!(text.contains("reasoning is carried back"), replayed);
        let without_opt_in =
            default_system_prompt_with_tools("/compat/project", replayed, &["todo".to_owned()]);
        assert!(!without_opt_in.contains("todo tool"));
    }
}

#[test]
fn explicit_metadata_is_trimmed_filtered_and_deduplicated_in_active_order() {
    let metadata = BTreeMap::from([
        (
            "first".to_owned(),
            ToolPromptMetadata {
                prompt_snippet: Some(" \r\n First\t  summary  \n".into()),
                prompt_guidelines: vec![
                    " Shared rule ".into(),
                    "".into(),
                    "First rule".into(),
                    "Be concise in your responses".into(),
                ],
            },
        ),
        (
            "second".to_owned(),
            ToolPromptMetadata {
                prompt_snippet: Some("Second summary".into()),
                prompt_guidelines: vec!["Shared rule".into(), "Second rule".into()],
            },
        ),
        (
            "inactive".to_owned(),
            ToolPromptMetadata {
                prompt_snippet: Some("Inactive summary".into()),
                prompt_guidelines: vec!["Inactive rule".into()],
            },
        ),
    ]);
    for replayed in [false, true] {
        let text = default_system_prompt_with_tool_prompts(
            "/compat/project",
            replayed,
            &["second".into(), "first".into()],
            &metadata,
        );
        assert!(text.contains("- second — Second summary\n- first — First summary"));
        assert!(text.contains("Guidelines:\n- Shared rule\n- Second rule\n- First rule"));
        assert_eq!(text.matches("Shared rule").count(), 1);
        assert_eq!(text.matches("Be concise in your responses").count(), 1);
        assert!(!text.contains("Inactive"));
        assert!(!text.contains("\n- \n"));
    }
}

#[test]
fn extension_metadata_replaces_builtin_metadata_even_when_empty() {
    for snippet in [
        None,
        Some(" \n\t ".to_owned()),
        Some(" Custom reader ".to_owned()),
    ] {
        let metadata = BTreeMap::from([(
            "read".to_owned(),
            ToolPromptMetadata {
                prompt_snippet: snippet.clone(),
                prompt_guidelines: vec![" Custom reader rule ".into()],
            },
        )]);
        let text = default_system_prompt_with_tool_prompts(
            "/compat/project",
            true,
            &["read".into()],
            &metadata,
        );
        assert!(!text.contains("Read file contents"));
        assert!(text.contains("- Custom reader rule"));
        if snippet
            .as_deref()
            .is_some_and(|snippet| !snippet.trim().is_empty())
        {
            assert!(text.contains("- read  — Custom reader"));
        } else {
            assert!(text.contains("Available tools:\n(none)\n\n"));
        }
    }
}
