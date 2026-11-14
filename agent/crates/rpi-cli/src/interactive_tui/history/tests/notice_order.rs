//! Runtime notices must scroll with replies, rather than follow the live slot.
use super::*;

fn failed_message(reason: rpi_ai::types::StopReason, error: &str) -> AgentMessage {
    AgentMessage::Assistant(Box::new(AssistantMessage {
        role: Default::default(),
        content: vec![],
        api: rpi_ai::Api::AnthropicMessages,
        provider: "synthetic".into(),
        model: "synthetic".into(),
        response_model: None,
        response_id: None,
        usage: rpi_ai::types::Usage::zero(),
        stop_reason: reason,
        deferred: None,
        error_message: Some(error.into()),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }))
}

#[tokio::test]
async fn runtime_errors_and_theme_notices_scroll_above_later_replies() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    for width in [28, 100] {
        let (_, state, tui) = fixture(0).await;
        state.history_page.mount_active(&state.chat_container);
        state.transcript_view().add_user("first-prompt");
        let notify = theme_watch_notify(&state, tui.clone());
        notify(Ok("Theme set to Dark (saved)".into()));
        for (reason, error) in [
            (
                rpi_ai::types::StopReason::Aborted,
                "abort error: Request was aborted",
            ),
            (
                rpi_ai::types::StopReason::Error,
                "http transport error: connection closed",
            ),
        ] {
            handle_agent_event(
                AgentEvent::MessageEnd {
                    message_id: None,
                    message: failed_message(reason, error),
                },
                &tui,
                &state,
                &state.chat_container,
                &lane,
            )
            .await;
        }
        state.transcript_view().add_user("next-prompt");
        let output = (0..40)
            .map(|index| format!("reply-line-{index:02}\n\n"))
            .collect::<String>();
        state
            .transcript_view()
            .apply(&UiEvent::AssistantStart { blocks: vec![] }, width);
        state.transcript_view().apply(
            &UiEvent::AssistantUpdate {
                blocks: vec![AssistantBlock::Text(output)],
                tool_calls: vec![],
            },
            width,
        );
        let full = strip_ansi(&state.chat_container.render(width).join("\n"));
        let positions: Vec<_> = [
            "first-prompt",
            "Theme set to Dark",
            "abort error:",
            "http transport error:",
            "next-prompt",
            "reply-line-39",
        ]
        .iter()
        .map(|marker| full.find(marker).unwrap())
        .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{full}");
        let scroll = ScrollView::new(
            state.chat_container.clone(),
            ScrollViewOptions {
                follow: FollowMode::End,
                scrollbar: ScrollbarMode::Hidden,
                ..Default::default()
            },
        );
        let tail = strip_ansi(&scroll.render_with_viewport(width, 8).join("\n"));
        assert!(tail.contains("reply-line-39"), "{tail}");
        for marker in ["Theme set", "abort error", "http transport"] {
            assert!(!tail.contains(marker), "{tail}");
        }
        scroll.scroll_to_start();
        let top = strip_ansi(&scroll.render_with_viewport(width, 20).join("\n"));
        assert!(top.contains("Theme set"), "{top}");
        assert!(top.contains("abort error"), "{top}");
    }
}

#[tokio::test]
async fn settled_errors_replay_once_and_runtime_notices_do_not_remain_at_the_root() {
    let (storage, state, tui) = fixture(80).await;
    state.history_page.mount_active(&state.chat_container);
    let notify = theme_watch_notify(&state, tui.clone());
    notify(Err("synthetic-theme-error".into()));
    add_error_message(&state.notice_container(), "synthetic-provider-error");
    append(
        &storage,
        80,
        ProvisionedKind::Message {
            message: failed_message(rpi_ai::types::StopReason::Error, "synthetic-provider-error"),
            terminate: None,
        },
    )
    .await;
    settle(&state, &tui);
    finish_request(&state).await;
    state.transcript_view().add_user("prompt-after-settlement");
    let content = text(&state);
    assert_eq!(content.matches("synthetic-provider-error").count(), 1);
    assert!(!content.contains("synthetic-theme-error"), "{content}");
    assert!(content.find("synthetic-provider-error") < content.find("prompt-after-settlement"));
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert!(!text(&state).contains("synthetic-provider-error"));
    assert!(!text(&state).contains("synthetic-theme-error"));
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert_eq!(text(&state).matches("synthetic-provider-error").count(), 1);
    state.reset_session_state();
    state.chat_container.clear();
    state.history_page.mount_active(&state.chat_container);
    assert!(!text(&state).contains("synthetic-"));
}

#[test]
fn unbound_transcripts_keep_runtime_messages_in_the_flat_chat() {
    let state = super::super::super::tests::test_tui_state();
    add_error_message(&state.notice_container(), "flat-error");
    add_note_message(&state.notice_container(), "flat-note");
    state.transcript_view().add_user("flat-next-prompt");
    let content = text(&state);
    assert!(content.find("flat-error") < content.find("flat-note"));
    assert!(content.find("flat-note") < content.find("flat-next-prompt"));
}

#[tokio::test]
async fn command_notices_remain_visible_on_older_history_pages() {
    let (_, state, tui) = fixture(80).await;
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    let notify = theme_watch_notify(&state, tui.clone());
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert!(!state.history_page.is_latest());
    let retired_page_notices = state.notice_container();
    notify(Ok("old-page-theme-notice".into()));
    notify(Err("old-page-theme-error".into()));
    copy_last_assistant(&state);
    let content = text(&state);
    assert!(content.contains("Nothing to copy yet"), "{content}");
    assert!(content.contains("old-page-theme-notice"), "{content}");
    assert!(content.contains("old-page-theme-error"), "{content}");
    assert!(content.contains("history-entry-000"));
    assert!(!content.contains("history-entry-079"));
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert!(!text(&state).contains("Nothing to copy yet"));
    assert!(!text(&state).contains("old-page-theme"));
    // A callback holding the old destination must not append behind live output.
    add_error_message(&retired_page_notices, "late-old-page-error");
    assert!(!text(&state).contains("late-old-page-error"));
    notify(Ok("latest-page-theme-notice".into()));
    state
        .transcript_view()
        .add_user("next-prompt-after-navigation");
    let content = text(&state);
    assert!(
        content.find("latest-page-theme-notice").unwrap()
            < content.find("next-prompt-after-navigation").unwrap()
    );
}

#[test]
fn theme_watcher_does_not_retain_the_session_state() {
    let state = super::super::super::tests::test_tui_state();
    let weak = Arc::downgrade(&state);
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let notify = theme_watch_notify(&state, tui);
    drop(state);
    assert!(weak.upgrade().is_none());
    notify(Err("late-notice-after-session-drop".into()));
}
