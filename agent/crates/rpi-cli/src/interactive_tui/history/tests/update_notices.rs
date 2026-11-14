//! Synthetic update cards and streaming components only; no network/user data.
use super::*;

fn report() -> crate::updates::UpdateReport {
    crate::updates::UpdateReport {
        notices: vec![crate::updates::UpdateNotice {
            name: "rpi-im-message".into(),
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            command: "rpi extension update rpi-im-message".into(),
        }],
        warnings: Vec::new(),
    }
}

#[test]
fn startup_update_card_scrolls_above_streaming_output_even_if_check_finishes_late() {
    for width in [28, 100] {
        for late_notice in [false, true] {
            let history = HistoryUi::default();
            let chat = Arc::new(Container::new());
            // Reserve the slot before the async check has any content.
            history.mount_active(&chat);
            let scroll = ScrollView::new(
                chat.clone(),
                ScrollViewOptions {
                    follow: FollowMode::End,
                    scrollbar: ScrollbarMode::Hidden,
                    ..Default::default()
                },
            );
            let assistant = Arc::new(AssistantMessageComponent::new(
                AssistantMessageOptions::default(),
            ));
            assistant.set_streaming(true);
            if late_notice {
                history.active.add_child(assistant.clone());
                assistant.update_blocks(&[AssistantBlock::Text("first-chunk".into())]);
                let _ = scroll.render_with_viewport(width, 8);
            }
            add_update_notices(&history.updates, &report());
            if !late_notice {
                history.active.add_child(assistant.clone());
            }
            let mut output = String::new();
            for index in 0..40 {
                output.push_str(&format!("stream-line-{index:02}\n\n"));
                assistant.update_blocks(&[AssistantBlock::Text(output.clone())]);
                let full = strip_ansi(&chat.render(width).join("\n"));
                let card = full.find("Package Update Available").unwrap();
                let stream = full.find("stream-line-00").unwrap();
                assert!(
                    card < stream,
                    "notice must precede streaming output: {full}"
                );
                let _ = scroll.render_with_viewport(width, 8);
            }
            let tail = strip_ansi(&scroll.render_with_viewport(width, 8).join("\n"));
            assert!(tail.contains("stream-line-39"), "{tail}");
            assert!(!tail.contains("Package Update Available"), "{tail}");
            // The box scrolled into history, rather than being deleted or pinned.
            scroll.scroll_to_start();
            let top = strip_ansi(&scroll.render_with_viewport(width, 8).join("\n"));
            assert!(top.contains("Package Update Available"), "{top}");
            assert!(!top.contains("stream-line-39"), "{top}");
        }
    }
}

#[tokio::test]
async fn update_card_survives_history_rebuilds_once_before_the_page_and_live_slot() {
    let (storage, state, tui) = fixture(80).await;
    restore_bound(&state, &state.chat_container).await;
    add_update_notices(&state.history_page.updates, &report());
    for direction in [Direction::Older, Direction::Latest] {
        request(state.clone(), tui.clone(), direction);
        finish_request(&state).await;
        let content = text(&state);
        assert_eq!(content.matches("Package Update Available").count(), 1);
        assert!(content.find("Package Update Available") < content.find("history-entry-"));
    }
    state.set_status(RunStatus::Working);
    state.transcript_view().add_user("live-user-after-update");
    let content = text(&state);
    assert!(content.find("Package Update Available") < content.find("live-user-after-update"));
    append(
        &storage,
        80,
        ProvisionedKind::Message {
            message: AgentMessage::User(UserMessage {
                role: Default::default(),
                timestamp: 80,
                content: rpi_ai::types::UserContent::Text("settled-user-after-update".into()),
            }),
            terminate: None,
        },
    )
    .await;
    state.set_status(RunStatus::Idle);
    settle(&state, &tui);
    finish_request(&state).await;
    restore_bound(&state, &state.chat_container).await;
    let content = text(&state);
    assert_eq!(content.matches("Package Update Available").count(), 1);
    assert!(content.find("Package Update Available") < content.find("settled-user-after-update"));
    assert!(!content.contains("live-user-after-update"));
}

#[test]
fn reset_clears_startup_update_cards_and_repeated_mounts_do_not_duplicate_them() {
    let history = HistoryUi::default();
    let chat = Arc::new(Container::new());
    for _ in 0..3 {
        history.mount_active(&chat);
    }
    add_update_notices(&history.updates, &report());
    assert_eq!(chat.child_count(), 3);
    assert_eq!(
        strip_ansi(&chat.render(100).join("\n"))
            .matches("Package Update Available")
            .count(),
        1
    );
    history.reset();
    assert!(!strip_ansi(&chat.render(100).join("\n")).contains("Package Update Available"));
}
