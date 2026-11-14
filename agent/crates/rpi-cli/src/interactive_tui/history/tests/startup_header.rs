//! Startup chrome is scrollable transcript content, not a disposable live page.
use super::*;

fn startup_header(history: &HistoryUi) -> Arc<Container> {
    let context = add_welcome_message_with_capabilities(
        &history.header,
        &["read".into(), "bash".into()],
        &["synthetic-skill".into()],
        &["SYNTHETIC.md".into()],
    );
    add_note_message(&history.header, "synthetic-startup-notice");
    context
}

#[tokio::test]
async fn first_and_second_reply_keep_startup_logo_tools_and_notes_in_scroll_history() {
    let (storage, state, tui) = fixture(0).await;
    startup_header(&state.history_page);
    // Fresh launches mount the live slot without restoring any persisted page.
    state.history_page.mount_active(&state.chat_container);
    let header = strip_ansi(&state.history_page.header.render(100).join("\n"));
    assert!(header.contains("██"));
    assert!(header.contains("Tools (2) read · bash"));
    assert!(header.contains("Skills (1) synthetic-skill"));
    assert!(header.contains("Context (1)"));
    assert!(text(&state).starts_with(&header));

    for turn in 0..2 {
        let prompt = format!("synthetic-prompt-{turn}");
        let reply = format!("synthetic-reply-{turn}");
        state.set_status(RunStatus::Working);
        state.transcript_view().add_user(&prompt);
        append(
            &storage,
            turn * 2,
            ProvisionedKind::Message {
                message: AgentMessage::User(UserMessage {
                    role: Default::default(),
                    timestamp: turn as i64,
                    content: rpi_ai::types::UserContent::Text(prompt),
                }),
                terminate: None,
            },
        )
        .await;
        for event in [
            UiEvent::AssistantStart { blocks: vec![] },
            UiEvent::AssistantUpdate {
                blocks: vec![AssistantBlock::Text(reply.clone())],
                tool_calls: vec![],
            },
            UiEvent::AssistantEnd {
                blocks: vec![AssistantBlock::Text(reply.clone())],
            },
        ] {
            state.transcript_view().apply(&event, 100);
            assert!(text(&state).starts_with(&header));
        }
        append(
            &storage,
            turn * 2 + 1,
            ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(AssistantMessage {
                    role: Default::default(),
                    content: vec![Content::text(&reply)],
                    api: rpi_ai::Api::AnthropicMessages,
                    provider: "synthetic".into(),
                    model: "synthetic".into(),
                    response_model: None,
                    response_id: None,
                    usage: rpi_ai::types::Usage::zero(),
                    stop_reason: rpi_ai::types::StopReason::Stop,
                    deferred: None,
                    error_message: None,
                    raw_stop_reason: None,
                    end_turn: None,
                    timestamp: turn as i64,
                })),
                terminate: None,
            },
        )
        .await;
        state.set_status(RunStatus::Idle);
        settle(&state, &tui);
        finish_request(&state).await;
        assert_eq!(state.history_page.active.child_count(), 0);
        let content = text(&state);
        assert!(content.starts_with(&header), "{content}");
        assert_eq!(content.matches("Tools (2)").count(), 1);
        assert_eq!(content.matches("synthetic-startup-notice").count(), 1);
        assert_eq!(content.matches(&reply).count(), 1);
    }
    for width in [28, 100] {
        let scroll = ScrollView::new(
            state.chat_container.clone(),
            ScrollViewOptions {
                follow: FollowMode::End,
                scrollbar: ScrollbarMode::Hidden,
                ..Default::default()
            },
        );
        let tail = strip_ansi(&scroll.render_with_viewport(width, 8).join("\n"));
        assert!(tail.contains("synthetic-reply-1"), "{tail}");
        assert!(!tail.contains("██"), "logo should scroll offscreen: {tail}");
        scroll.scroll_to_start();
        let top = strip_ansi(&scroll.render_with_viewport(width, 8).join("\n"));
        assert!(
            top.contains("██"),
            "logo should remain in scrollback: {top}"
        );
    }
}

#[tokio::test]
async fn startup_context_handle_remains_visible_after_restore_and_history_paging() {
    let (_, state, tui) = fixture(80).await;
    let context = startup_header(&state.history_page);
    state.history_page.mount_active(&state.chat_container);
    restore_bound(&state, &state.chat_container).await;
    for direction in [Direction::Older, Direction::Latest] {
        request(state.clone(), tui.clone(), direction);
        finish_request(&state).await;
        assert_eq!(text(&state).matches("Tools (2)").count(), 1);
        assert_eq!(text(&state).matches("synthetic-startup-notice").count(), 1);
    }
    // /reload keeps this same listing handle; do not recreate a stale banner.
    render_context_files(&context, &["SYNTHETIC-RELOADED.md".into()]);
    assert!(text(&state).contains("SYNTHETIC-RELOADED.md"));
    assert!(!text(&state).contains("SYNTHETIC.md"));
    assert!(text(&state).contains("Tools (2) read · bash"));
}

#[tokio::test]
async fn startup_notes_without_a_welcome_banner_survive_settlement_and_reset_clears_them() {
    let (_, state, tui) = fixture(0).await;
    // Quiet startup omits the logo/capability listing, but not launch notices.
    add_note_message(&state.history_page.header, "synthetic-quiet-startup-notice");
    state.history_page.mount_active(&state.chat_container);
    assert!(text(&state).contains("synthetic-quiet-startup-notice"));
    settle(&state, &tui);
    finish_request(&state).await;
    let content = text(&state);
    assert_eq!(content.matches("synthetic-quiet-startup-notice").count(), 1);
    assert!(!content.contains("Tools ("));
    assert!(!content.contains("██"));
    state.history_page.reset();
    assert!(!text(&state).contains("synthetic-quiet-startup-notice"));
}
