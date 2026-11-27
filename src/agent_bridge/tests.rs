#[test]
fn automatic_approval_only_covers_existing_workspace_files() {
    let base = std::env::temp_dir().join(format!("shob-permissions-{}", std::process::id()));
    let workspace = base.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("file"), "test").unwrap();
    std::fs::write(base.join("outside"), "test").unwrap();
    let args = |path: &str| serde_json::json!({"path": path});
    assert!(super::workspace_edit("edit", &args("file"), &workspace));
    assert!(super::workspace_edit("write", &args("file"), &workspace));
    assert!(!super::workspace_edit("bash", &args("file"), &workspace));
    assert!(!super::workspace_edit("write", &args("new"), &workspace));
    assert!(!super::workspace_edit(
        "edit",
        &args("../outside"),
        &workspace
    ));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(base.join("outside"), workspace.join("link")).unwrap();
        assert!(!super::workspace_edit("edit", &args("link"), &workspace));
    }
    std::fs::remove_dir_all(base).unwrap();
}
#[test]
fn browser_mentions_require_installation_and_enrich_resumed_context() {
    let installed = std::path::Path::new("installed-agent-browser");
    for prompt in [
        "@browser open example.com",
        "Please use @browser, then summarize.",
    ] {
        assert_eq!(
            super::browser_prompt(prompt, None).unwrap_err(),
            crate::browser::INSTALL_REQUIRED,
        );
        let (prepared, context) = super::prepare_prompt(
            prompt,
            Some(format!("Prior conversation\n{prompt}")),
            Some(installed),
        )
        .unwrap();
        assert!(prepared.starts_with(prompt));
        assert!(prepared.contains("installed-agent-browser"));
        assert_eq!(context.unwrap(), format!("Prior conversation\n{prepared}"));
    }
    for prompt in [
        "hello",
        "user@browser.com",
        "@browser-extension",
        "@browser2",
    ] {
        assert_eq!(super::browser_prompt(prompt, None).unwrap(), prompt);
    }
}
use super::*;
use serde_json::json;

#[test]
fn only_settled_or_handled_prompts_complete() {
    assert!(decode_event(&json!({"type":"agent_end"})).is_empty());
    assert!(matches!(
        &decode_event(&json!({"type":"agent_settled"}))[..],
        [AgentEvent::Done]
    ));
    assert!(matches!(
        &decode_event(
            &json!({"type":"response","command":"prompt","success":true,"data":{"disposition":"handled"}})
        )[..],
        [AgentEvent::Done]
    ));
    assert!(matches!(
        &decode_event(&json!({"type":"response","command":"prompt","success":false,"error":"bad"}))
            [..],
        [AgentEvent::Error(_), AgentEvent::Done]
    ));
}

#[test]
fn full_and_delta_message_updates_apply_only_the_increment() {
    // Delta projection (`messageUpdates: "delta"`).
    let delta = json!({
        "type": "message_update",
        "message": {"role": "assistant"},
        "assistantMessageEvent": {"type": "text_delta", "delta": "abc"}
    });
    assert!(matches!(
        decode_message_update(&delta),
        Some(AgentEvent::Text(text)) if text == "abc"
    ));

    // Older/full projection: the accumulated message and partial are also
    // present. Re-applying them would duplicate the stream, so only the
    // increment is consumed.
    let full = json!({
        "type": "message_update",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "abcdef"}]},
        "assistantMessageEvent": {
            "type": "text_delta",
            "delta": "def",
            "partial": {"role": "assistant", "content": [{"type": "text", "text": "abcdef"}]}
        }
    });
    assert!(matches!(
        decode_message_update(&full),
        Some(AgentEvent::Text(text)) if text == "def"
    ));

    let thinking = json!({
        "type": "message_update",
        "message": {"role": "assistant"},
        "assistantMessageEvent": {"type": "thinking_delta", "delta": "hmm"}
    });
    assert!(matches!(
        decode_message_update(&thinking),
        Some(AgentEvent::Thinking(text)) if text == "hmm"
    ));
}

#[test]
fn coalescing_merges_deltas_and_keeps_structural_events() {
    let out = coalesce_events(vec![
        AgentEvent::Text("a".into()),
        AgentEvent::Text("b".into()),
        AgentEvent::Thinking("c".into()),
        AgentEvent::Thinking("d".into()),
        AgentEvent::ToolStart {
            id: Some("t".into()),
            name: "bash".into(),
            input: "{}".into(),
        },
        AgentEvent::ToolUpdate {
            id: Some("t".into()),
            name: "bash".into(),
            output: "1".into(),
        },
        AgentEvent::ToolUpdate {
            id: Some("t".into()),
            name: "bash".into(),
            output: "12".into(),
        },
        AgentEvent::ToolEnd {
            id: Some("t".into()),
            name: "bash".into(),
            output: "12".into(),
            failed: false,
        },
        AgentEvent::Done,
    ]);
    assert_eq!(out.len(), 6);
    assert!(matches!(&out[0], AgentEvent::Text(text) if text == "ab"));
    assert!(matches!(&out[1], AgentEvent::Thinking(text) if text == "cd"));
    assert!(matches!(&out[2], AgentEvent::ToolStart { .. }));
    // Only the newest cumulative snapshot survives.
    assert!(matches!(&out[3], AgentEvent::ToolUpdate { output, .. } if output == "12"));
    assert!(matches!(&out[4], AgentEvent::ToolEnd { .. }));
    assert!(matches!(&out[5], AgentEvent::Done));
}

#[test]
fn every_antigravity_model_uses_the_embedded_approval_path() {
    assert!(uses_gemini(Some("google-antigravity/gemini-3.8-flash")));
    assert!(uses_gemini(Some(
        "google-antigravity/gemini-3.1-flash-lite"
    )));
    assert!(uses_gemini(Some("google-antigravity/claude-sonnet-4-6")));
    assert!(uses_gemini(Some("google/gemini-3.8-flash")));
    assert!(!uses_gemini(Some("openai-codex/gpt-6.1-sol")));
}

#[test]
fn coalescing_does_not_cross_structural_events_or_the_entry_cap() {
    let out = coalesce_events(vec![
        AgentEvent::Text("a".into()),
        AgentEvent::Status("s".into()),
        AgentEvent::Text("b".into()),
    ]);
    assert_eq!(out.len(), 3);

    let half = "x".repeat(COALESCE_TEXT_LIMIT / 2 + 1);
    let out = coalesce_events(vec![AgentEvent::Text(half.clone()), AgentEvent::Text(half)]);
    assert_eq!(out.len(), 2, "merged text must stay under the entry cap");
}

#[test]
fn unidentified_snapshots_and_structural_boundaries_are_lossless() {
    let update = |id: Option<&str>, output: &str| AgentEvent::ToolUpdate {
        id: id.map(str::to_owned),
        name: "bash".into(),
        output: output.into(),
    };
    let out = coalesce_events(vec![
        update(None, "first"),
        update(None, "second"),
        update(Some("a"), "old"),
        update(Some("b"), "peer"),
        update(Some("a"), "latest"),
        AgentEvent::Error("boundary".into()),
        update(Some("a"), "after"),
        AgentEvent::ToolEnd {
            id: Some("a".into()),
            name: "bash".into(),
            output: "final".into(),
            failed: false,
        },
        AgentEvent::Done,
    ]);
    let snapshots: Vec<_> = out
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolUpdate { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots, ["first", "second", "peer", "latest", "after"]);
    assert!(matches!(&out[4], AgentEvent::Error(text) if text == "boundary"));
    assert!(matches!(&out[6], AgentEvent::ToolEnd { output, .. } if output == "final"));
    assert!(matches!(out.last(), Some(AgentEvent::Done)));
}

#[test]
fn composer_tool_hook_auto_allows_subagent_calls() {
    assert!(auto_allowed_tool(embedded::subagent_tool::NAME));
    assert!(auto_allowed_tool(embedded::task_list::NAME));
    assert!(!auto_allowed_tool("bash"));
}
