//! In-memory only: no provider, configuration or real sessions.
use super::*;
use rpi_harness::session::types::{ProvisionedEntry, ProvisionedKind, SessionMetadata};
use rpi_harness::session::{CounterIdGenerator, FakeClock, InMemorySessionStorage};

#[path = "tests/startup_header.rs"]
mod startup_header;
#[path = "tests/update_notices.rs"]
mod update_notices;

async fn fixture(
    count: usize,
) -> (
    Arc<InMemorySessionStorage>,
    Arc<TuiState>,
    Arc<TuiAltScreen>,
) {
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "tui-history-test".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    ));
    for index in 0..count {
        append(
            &storage,
            index,
            ProvisionedKind::Message {
                message: AgentMessage::User(UserMessage {
                    role: Default::default(),
                    timestamp: index as i64,
                    content: rpi_ai::types::UserContent::Text(format!("history-entry-{index:03}")),
                }),
                terminate: None,
            },
        )
        .await;
    }
    let state = super::super::tests::test_tui_state();
    state.history_page.resident.lock().unwrap().storage = Some(storage.clone());
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    (storage, state, tui)
}
async fn append(storage: &InMemorySessionStorage, index: usize, kind: ProvisionedKind) {
    storage
        .append_entry(
            ProvisionedEntry {
                id: format!("entry-{index:03}"),
                kind,
            },
            "main",
        )
        .await
        .unwrap();
}
async fn finish_request(state: &Arc<TuiState>) {
    let handle = state.history_page.worker.lock().unwrap().take();
    if let Some(handle) = handle {
        handle.await.unwrap();
    }
}
fn text(state: &TuiState) -> String {
    strip_ansi(&state.chat_container.render(100).join("\n"))
}

#[test]
fn update_notices_mount_before_live_transcript_slot() {
    let history = HistoryUi::default();
    let chat = Arc::new(Container::new());
    history.mount_active(&chat);
    add_update_notices(
        &history.updates,
        &crate::updates::UpdateReport {
            notices: vec![crate::updates::UpdateNotice {
                name: "rpi-im-message".into(),
                current: "0.1.0".into(),
                latest: "0.2.0".into(),
                command: "rpi extension update rpi-im-message".into(),
            }],
            warnings: Vec::new(),
        },
    );

    let children = chat.get_children();
    assert_eq!(children.len(), 3);
    let updates: Arc<dyn Component> = history.updates.clone();
    let active: Arc<dyn Component> = history.active.clone();
    let panels: Arc<dyn Component> = history.panels.clone();
    assert!(Arc::ptr_eq(&children[0], &updates));
    assert!(Arc::ptr_eq(&children[1], &active));
    assert!(Arc::ptr_eq(&children[2], &panels));
    assert!(strip_ansi(&chat.render(100).join("\\n")).contains("Package Update Available"));
}

#[tokio::test]
async fn pages_replace_not_accumulate_and_release_old_components_and_search() {
    let (_, state, tui) = fixture(123).await;
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert!(text(&state).contains("history-entry-122"));
    assert!(!text(&state).contains("history-entry-072"));
    let children = state.chat_container.get_children();
    let old_page = Arc::downgrade(&children[1]);
    drop(children);
    state.search.activate();
    state.search.set_query("history-entry-122");
    state.search.find_matches(&["history-entry-122".into()]);
    state.tool_components.lock().unwrap().insert(
        "old-tool".into(),
        Arc::new(ToolExecutionComponent::new("read", "old")),
    );
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert!(old_page.upgrade().is_none());
    assert!(state.tool_components.lock().unwrap().is_empty());
    assert_eq!(state.search.get_match_count(), 0);
    assert!(!state.search.is_active());
    assert!(text(&state).contains("history-entry-023"));
    assert!(text(&state).contains("history-entry-072"));
    assert!(!text(&state).contains("history-entry-073"));
    request(state.clone(), tui.clone(), Direction::Newer);
    finish_request(&state).await;
    assert!(text(&state).contains("history-entry-122"));
    assert!(!text(&state).contains("history-entry-072"));
    assert_eq!(state.chat_container.child_count(), 4);
}

#[tokio::test]
async fn invalid_cursor_preserves_page_and_stale_generation_or_storage_never_matches() {
    let (storage, state, tui) = fixture(70).await;
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    let original = state.chat_container.get_children()[1].clone();
    state
        .history_page
        .resident
        .lock()
        .unwrap()
        .page
        .as_mut()
        .unwrap()
        .next_before_id = Some("missing-cursor".into());
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert!(Arc::ptr_eq(
        &original,
        &state.chat_container.get_children()[1]
    ));
    assert!(text(&state).contains("history-entry-069"));
    assert!(text(&state).contains("Could not load history"));
    let bound: Arc<dyn SessionStorage> = storage;
    let generation = state.history_page.resident.lock().unwrap().generation;
    assert!(current(
        &state.history_page.resident.lock().unwrap(),
        generation,
        &bound
    ));
    state.history_page.invalidate();
    assert!(!current(
        &state.history_page.resident.lock().unwrap(),
        generation,
        &bound
    ));
    let (other, _, _) = fixture(1).await;
    let other: Arc<dyn SessionStorage> = other;
    let resident = state.history_page.resident.lock().unwrap();
    assert!(!current(&resident, resident.generation, &other));
}

#[tokio::test]
async fn latest_append_settlement_is_bounded_and_old_pages_do_not_revert_panels() {
    let (storage, state, tui) = fixture(75).await;
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    crate::transcript_view::update_plan_panel(
        &state.history_page.panels,
        &serde_json::json!({"details":{"kind":"plan","active":true,"plan":"current-plan"}}),
    );
    crate::transcript_view::update_todo_list(
        &state.history_page.panels,
        &serde_json::json!({"details":{"todos":[{"text":"current-task","done":false}]}}),
    );
    for index in 75..145 {
        append(
            &storage,
            index,
            ProvisionedKind::Message {
                message: AgentMessage::User(UserMessage {
                    role: Default::default(),
                    timestamp: index as i64,
                    content: rpi_ai::types::UserContent::Text(format!("history-entry-{index:03}")),
                }),
                terminate: None,
            },
        )
        .await;
        add_user_message(&state.history_page.active, "live-only");
    }
    settle(&state, &tui);
    finish_request(&state).await;
    assert!(state.history_page.active.get_children().is_empty());
    assert!(text(&state).contains("history-entry-144"));
    assert!(!text(&state).contains("history-entry-094"));
    assert!(!text(&state).contains("live-only"));
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    let old = text(&state);
    state.set_status(RunStatus::Working);
    state.transcript_view().add_user("detached-live-message");
    assert!(!text(&state).contains("detached-live-message"));
    state.set_status(RunStatus::Idle);
    settle(&state, &tui);
    assert_eq!(old, text(&state));
    assert!(state.history_page.active.get_children().is_empty());
    assert_eq!(state.history_page.panels.child_count(), 2);
}

#[tokio::test]
async fn byte_budget_keeps_oversized_entries_whole_and_all_variants_page() {
    let (storage, _, _) = fixture(61).await;
    let kinds = [
        ProvisionedKind::ModelChange {
            provider: "test".into(),
            model_id: "model".into(),
        },
        ProvisionedKind::ThinkingLevel {
            thinking_level: "high".into(),
        },
        ProvisionedKind::ActiveTools {
            active_tool_names: vec!["read".into()],
        },
        ProvisionedKind::Compaction {
            summary: "compact".into(),
            retained_tail: vec![],
            tokens_before: 12,
            details: None,
            usage: None,
        },
        ProvisionedKind::BranchSummary {
            from_id: "entry-000".into(),
            summary: "branch".into(),
            details: None,
            usage: None,
        },
        ProvisionedKind::Custom {
            custom_type: "test".into(),
            data: Some(serde_json::json!({"text":"x".repeat(2048)})),
        },
    ];
    for (offset, kind) in kinds.into_iter().enumerate() {
        append(&storage, 61 + offset, kind).await;
    }
    let session = Session::new(storage, None);
    let page = session
        .branch_entry_page(&BranchEntryPageQuery {
            max_bytes: 16,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    let Entry::Custom(custom) = &page.entries[0] else {
        panic!("whole custom entry expected")
    };
    assert_eq!(
        custom.data.as_ref().unwrap()["text"]
            .as_str()
            .unwrap()
            .len(),
        2048
    );
    let page = session
        .branch_entry_page(&Default::default())
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 50);
    for variant in [
        "message",
        "model_change",
        "thinking_level_change",
        "active_tools_change",
        "compaction",
        "branch_summary",
        "custom",
    ] {
        assert!(
            page.entries
                .iter()
                .any(|entry| entry.entry_type() == variant),
            "{variant}"
        );
    }
    let chat = Arc::new(Container::new());
    render_history_entries(page.entries, &chat, None, None, false);
    let output = strip_ansi(&chat.render(100).join("\n"));
    assert!(output.contains("Model changed"));
    assert!(output.contains("Thinking level"));
    assert!(output.contains("Compacted"));
    assert!(output.contains("Branch summary"));
}

#[tokio::test]
async fn repeated_restore_replaces_the_page_and_mounts_live_user_messages() {
    let (_, state, _) = fixture(80).await;
    restore_bound(&state, &state.chat_container).await;
    let old_page = Arc::downgrade(&state.chat_container.get_children()[1]);
    restore_bound(&state, &state.chat_container).await;
    assert!(old_page.upgrade().is_none());
    assert_eq!(state.chat_container.child_count(), 4);
    state.set_status(RunStatus::Working);
    state.transcript_view().add_user("visible-live-user");
    assert!(text(&state).contains("visible-live-user"));
    assert_eq!(text(&state).matches("history-entry-079").count(), 1);
}

#[tokio::test]
async fn empty_restore_mounts_live_slot_and_first_prompt_is_visible() {
    let (_, state, _) = fixture(0).await;
    restore_bound(&state, &state.chat_container).await;
    state.transcript_view().add_user("first-prompt");
    assert!(text(&state).contains("first-prompt"));
    state.history_page.mount_active(&state.chat_container);
    assert_eq!(text(&state).matches("first-prompt").count(), 1);
}

#[tokio::test]
async fn repeated_page_failures_keep_one_notice_and_preserve_the_page() {
    let (_, state, tui) = fixture(80).await;
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    let page = state.chat_container.get_children()[1].clone();
    state
        .history_page
        .resident
        .lock()
        .unwrap()
        .page
        .as_mut()
        .unwrap()
        .next_before_id = Some("invalid-cursor".into());
    for _ in 0..8 {
        request(state.clone(), tui.clone(), Direction::Older);
        finish_request(&state).await;
        assert!(Arc::ptr_eq(&page, &state.chat_container.get_children()[1]));
        assert_eq!(state.chat_container.child_count(), 5);
        assert_eq!(text(&state).matches("Could not load history").count(), 1);
    }
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert_eq!(state.chat_container.child_count(), 4);
    assert!(!text(&state).contains("Could not load history"));
}

#[tokio::test]
async fn old_page_settlement_preserves_running_bash_and_unsaved_active_data() {
    let (_, state, tui) = fixture(80).await;
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    let original = text(&state);
    let _cancel = state.begin_user_bash();
    add_user_message(&state.history_page.active, "not-saved-yet");
    settle(&state, &tui);
    assert_eq!(state.history_page.active.child_count(), 2);
    assert_eq!(text(&state), original);
    state.finish_user_bash();
    state
        .history_page
        .unsaved
        .store(true, std::sync::atomic::Ordering::SeqCst);
    settle(&state, &tui);
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert_eq!(state.history_page.active.child_count(), 2);
    assert_eq!(text(&state), original);
    state
        .history_page
        .unsaved
        .store(false, std::sync::atomic::Ordering::SeqCst);
    settle(&state, &tui);
    assert_eq!(state.history_page.active.child_count(), 0);
}

#[tokio::test]
async fn bounded_panel_restore_uses_sequence_order_across_legacy_todo_kinds() {
    let (storage, state, tui) = fixture(80).await;
    for (index, kind, data) in [
        (
            80,
            "todo-state",
            serde_json::json!({"todos":[{"text":"obsolete-task","done":false}]}),
        ),
        (
            81,
            "todo",
            serde_json::json!({"todos":[{"text":"newest-task","done":false}]}),
        ),
        (
            82,
            "plan-mode",
            serde_json::json!({"active":true,"plan":"restored-plan"}),
        ),
    ] {
        append(
            &storage,
            index,
            ProvisionedKind::Custom {
                custom_type: kind.into(),
                data: Some(data),
            },
        )
        .await;
    }
    let tree = Session::new(storage, None).view("main");
    restore_history_panels(&tree, &state.history_page.panels).await;
    let panel_text = strip_ansi(&state.history_page.panels.render(100).join("\n"));
    assert!(panel_text.contains("newest-task"));
    assert!(!panel_text.contains("obsolete-task"));
    assert_eq!(state.history_page.panels.child_count(), 2);
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert_eq!(
        panel_text,
        strip_ansi(&state.history_page.panels.render(100).join("\n"))
    );
}

#[tokio::test]
async fn request_works_while_run_is_active_and_keeps_active_maps_independent() {
    let (_, state, tui) = fixture(80).await;
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    state.set_status(RunStatus::Working);
    state.transcript_view().add_user("active-operation");
    let component = Arc::new(ToolExecutionComponent::new("read", "active"));
    state
        .tool_components
        .lock()
        .unwrap()
        .insert("live-tool".into(), component.clone());
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert!(text(&state).contains("history-entry-000"));
    assert!(!text(&state).contains("active-operation"));
    assert!(Arc::ptr_eq(
        &component,
        &state.tool_components.lock().unwrap()["live-tool"]
    ));
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert!(text(&state).contains("active-operation"));
    assert!(state
        .tool_components
        .lock()
        .unwrap()
        .contains_key("live-tool"));
}

#[tokio::test]
async fn blocking_assistant_output_uses_detached_live_slot_on_old_pages() {
    let (_, state, tui) = fixture(80).await;
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    let old_page = text(&state);
    state.set_status(RunStatus::Working);
    add_assistant_message_for_state(&state, "blocking-live-answer");
    assert_eq!(text(&state), old_page);
    assert!(
        strip_ansi(&state.history_page.active.render(100).join("\n"))
            .contains("blocking-live-answer")
    );
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert!(text(&state).contains("blocking-live-answer"));
}

#[tokio::test]
async fn capability_header_survives_restore_and_page_replacement_once() {
    let (_, state, tui) = fixture(80).await;
    add_note_message(&state.history_page.header, "capability-header");
    restore_bound(&state, &state.chat_container).await;
    assert_eq!(text(&state).matches("capability-header").count(), 1);
    let nested = Arc::new(Container::new());
    let tool = Arc::new(ToolExecutionComponent::new("read", "history-tool"));
    nested.add_child(tool.clone());
    state.chat_container.add_child(nested.clone());
    assert!(state.toggle_tool_outputs());
    assert!(
        tool.is_expanded(),
        "toggles must reach nested historical components"
    );
    assert!(!state.toggle_tool_outputs());
    assert!(!tool.is_expanded());
    *state.tool_outputs_expanded.lock().unwrap() = true;
    state.apply_history_tool_expansion(&nested);
    assert!(
        tool.is_expanded(),
        "new pages must inherit the current preference"
    );
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert_eq!(text(&state).matches("capability-header").count(), 1);
    restore_bound(&state, &state.chat_container).await;
    assert_eq!(text(&state).matches("capability-header").count(), 1);
}

#[path = "tests/notice_order.rs"]
mod notice_order;
