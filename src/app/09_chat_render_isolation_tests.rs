#[cfg(test)]
mod chat_render_isolation_tests {
    use super::*;
    use gpui::{VisualContext, VisualTestContext};
    use resize_tests::{TestAppContext, open_app};

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.simulate_next_frame(cx);
        });
    }

    #[test]
    fn ordinary_deltas_render_only_chat_and_leave_composer_actionable() {
        use agent_bridge::AgentEvent;
        for focused_layout in [false, true] {
            let mut app = TestAppContext::new();
            let (mut cx, state) = open_app(&mut app);
            cx.update_window_entity(&state, |state, _, cx| {
                state.settings.agent_layout = focused_layout;
                state.projects[0].sessions[0].status = SessionStatus::Working;
                let chat = &mut state.pane_mut(1).unwrap().chat;
                chat.active = true;
                chat.running = true;
                chat.messages
                    .push(Arc::new(ChatMessage::new(ChatRole::User, "Hello".into())));
                cx.notify();
            });
            draw(&mut cx);
            let (view, input) = state.read_with(&cx, |state, _| {
                let pane = state.pane_ref(1).unwrap();
                (
                    pane.chat.view.borrow().as_ref().unwrap().clone(),
                    pane.input_state.as_ref().unwrap().clone(),
                )
            });
            cx.update(|window, cx| input.update(cx, |input, cx| input.focus(window, cx)));
            draw(&mut cx);
            cx.run_until_parked();
            draw(&mut cx);
            let notifications = Rc::new(std::cell::Cell::new(0usize));
            let observed_notifications = notifications.clone();
            let _observer = cx.update(|_, cx| {
                cx.observe(&state, move |_, _| {
                    observed_notifications.set(observed_notifications.get() + 1);
                })
            });

            let mut typed = String::new();
            for event in [
                AgentEvent::Thinking("Considering ".into()),
                AgentEvent::Thinking("the request.".into()),
                AgentEvent::Text("An answer ".into()),
                AgentEvent::Text("with **formatting**.".into()),
            ] {
                let event_label = format!("{event:?}");
                let notifications_before = notifications.get();
                let roots_before = APP_ROOT_RENDERS.with(|count| count.get());
                let chat_before = view.read_with(&cx, |view, _| view.renders);
                cx.update_window_entity(&state, |state, _, cx| {
                    assert!(!PaneChat::event_updates_root(&event));
                    let chat = &mut state.pane_mut(1).unwrap().chat;
                    chat.observe_status(&event);
                    push_chat_event(&mut chat.messages, event);
                    chat.active_dirty.set(true);
                    chat.notify_content(cx);
                });
                draw(&mut cx);
                assert_eq!(
                    APP_ROOT_RENDERS.with(|count| count.get()),
                    roots_before,
                    "content-only redraw: layout={focused_layout}, event={event_label}, root_notifications={}",
                    notifications.get() - notifications_before
                );
                assert!(
                    view.read_with(&cx, |view, _| view.renders) > chat_before,
                    "the independent chat view must actually render"
                );
                cx.simulate_input("x");
                draw(&mut cx);
                typed.push('x');
                assert_eq!(
                    input.read_with(&cx, |input, _| input.value().to_string()),
                    typed,
                    "the retained composer must continue accepting real input"
                );
            }
            cx.update_window_entity(&state, |state, _, cx| {
                state.pane_mut(1).unwrap().chat.active = false;
                cx.notify();
            });
            draw(&mut cx);
            let roots_before = APP_ROOT_RENDERS.with(|count| count.get());
            let chat_before = view.read_with(&cx, |view, _| view.renders);
            cx.update_window_entity(&state, |state, _, cx| {
                let chat = &mut state.pane_mut(1).unwrap().chat;
                let event = AgentEvent::Text(" Background continuation.".into());
                chat.observe_status(&event);
                push_chat_event(&mut chat.messages, event);
                chat.notify_content(cx);
            });
            draw(&mut cx);
            assert_eq!(APP_ROOT_RENDERS.with(|count| count.get()), roots_before);
            assert_eq!(view.read_with(&cx, |view, _| view.renders), chat_before);
            cx.update_window_entity(&state, |state, _, cx| {
                let chat = &mut state.pane_mut(1).unwrap().chat;
                chat.active = true;
                assert!(
                    chat.messages
                        .last()
                        .unwrap()
                        .text
                        .ends_with(" Background continuation.")
                );
                assert_eq!(
                    chat.view.borrow().as_ref().unwrap().entity_id(),
                    view.entity_id()
                );
                cx.notify();
            });
            draw(&mut cx);
            assert!(view.read_with(&cx, |view, _| view.renders) > chat_before);
        }
    }

    #[test]
    fn inactive_idle_views_unload_and_restore_without_losing_conversation_state() {
        let mut app = TestAppContext::new();
        let (mut cx, state) = open_app(&mut app);
        cx.update_window_entity(&state, |state, _, cx| {
            let pane = state.pane_mut(1).unwrap();
            pane.chat.active = true;
            pane.chat.shob_session_id = Some("saved-session".into());
            pane.chat.agent_draft = "Unsent draft".into();
            pane.chat.messages.push(Arc::new(ChatMessage::new(
                ChatRole::Assistant,
                "A **saved** answer.".into(),
            )));
            cx.notify();
        });
        draw(&mut cx);
        let first_view = state.read_with(&cx, |state, _| {
            let chat = &state.pane_ref(1).unwrap().chat;
            assert!(!chat.row_cache.borrow().rows.is_empty());
            chat.view.borrow().as_ref().unwrap().entity_id()
        });
        cx.update_window_entity(&state, |state, window, cx| {
            state.release_inactive_resources(cx);
            assert!(state.pane_ref(1).unwrap().chat.view.borrow().is_some());
            state.create_new_session_in_project(0, window, cx);
            let pane = state.pane_mut(1).unwrap();
            pane.chat.running = true;
            pane.status = SessionStatus::Working;
            state.release_inactive_resources(cx);
            assert!(state.pane_ref(1).unwrap().chat.view.borrow().is_some());
            let pane = state.pane_mut(1).unwrap();
            pane.chat.running = false;
            pane.status = SessionStatus::Blocked;
            state.release_inactive_resources(cx);
            assert!(state.pane_ref(1).unwrap().chat.view.borrow().is_some());
            let pane = state.pane_mut(1).unwrap();
            pane.status = SessionStatus::Done;
            pane.chat.follow_ups.push_back("Continue".into());
            state.release_inactive_resources(cx);
            assert!(state.pane_ref(1).unwrap().chat.view.borrow().is_some());
            state.pane_mut(1).unwrap().chat.follow_ups.clear();
            state.release_inactive_resources(cx);
            let pane = state.pane_ref(1).unwrap();
            assert!(pane.chat.view.borrow().is_none());
            assert!(pane.chat.row_cache.borrow().rows.is_empty());
            assert_eq!(pane.status, SessionStatus::Done);
            assert_eq!(pane.chat.shob_session_id.as_deref(), Some("saved-session"));
            assert_eq!(pane.chat.agent_draft, "Unsent draft");
        });
        draw(&mut cx);
        cx.update_window_entity(&state, |state, _, cx| {
            state.projects[0].active_session_idx = 0;
            cx.notify();
        });
        draw(&mut cx);
        state.read_with(&cx, |state, _| {
            let pane = state.pane_ref(1).unwrap();
            assert_ne!(
                pane.chat.view.borrow().as_ref().unwrap().entity_id(),
                first_view
            );
            assert!(!pane.chat.row_cache.borrow().rows.is_empty());
            assert_eq!(pane.chat.messages[0].text, "A **saved** answer.");
            assert_eq!(pane.chat.shob_session_id.as_deref(), Some("saved-session"));
            assert_eq!(pane.chat.agent_draft, "Unsent draft");
        });
        cx.update_window_entity(&state, |state, _, cx| {
            state.projects[0].active_session_idx = 1;
            state.release_inactive_resources(cx);
            assert!(state.pane_ref(1).unwrap().chat.view.borrow().is_none());
        });
    }

    #[test]
    fn quiet_background_pane_hibernates_and_wakes_without_touching_its_shell() {
        let mut app = TestAppContext::new();
        let (mut cx, state) = open_app(&mut app);
        cx.update_window_entity(&state, |state, window, cx| {
            // A pane that was looked at an hour ago and has been quiet since.
            let idle_since = std::time::Instant::now() - std::time::Duration::from_secs(3_600);
            let pane = state.pane_mut(1).unwrap();
            pane.lifecycle = session_lifecycle::PaneLifecycle::new(idle_since);
            pane.lifecycle.mark_seen(idle_since);
            // Hibernation must not start I/O that had not started yet, and must
            // not stop I/O that had: the shell outlives the grid either way.
            let started_before = pane.terminal.read(cx).is_started();

            state.create_new_session_in_project(0, window, cx);
            state.release_inactive_resources(cx);

            let pane = state.pane_ref(1).unwrap();
            assert!(pane.terminal.read(cx).is_hibernated());
            assert_eq!(
                pane.lifecycle.state(),
                session_lifecycle::LifecycleState::Hibernated
            );
            assert_eq!(pane.terminal.read(cx).is_started(), started_before);

            // Waking rebuilds the grid in place. Driving it directly keeps the
            // test from starting a real shell.
            pane.terminal.update(cx, |terminal, cx| terminal.wake(cx));
            let pane = state.pane_ref(1).unwrap();
            assert!(!pane.terminal.read(cx).is_hibernated());

            // Selecting it again heals the policy state on the next tick.
            state.projects[0].active_session_idx = 0;
            state.release_inactive_resources(cx);
            assert_eq!(
                state.pane_ref(1).unwrap().lifecycle.state(),
                session_lifecycle::LifecycleState::Active
            );

            // And it can go back to sleep once it is quiet again: hibernation
            // is a state, not a one-way transition.
            state.projects[0].active_session_idx = 1;
            {
                let pane = state.pane_mut(1).unwrap();
                let quiet = std::time::Instant::now() - std::time::Duration::from_secs(3_600);
                pane.lifecycle = session_lifecycle::PaneLifecycle::new(quiet);
                pane.lifecycle.mark_seen(quiet);
            }
            state.release_inactive_resources(cx);
            assert!(state.pane_ref(1).unwrap().terminal.read(cx).is_hibernated());
        });
    }

    #[test]
    fn dormant_terminal_input_starts_io_and_completes_without_render() {
        struct Output(Arc<std::sync::atomic::AtomicUsize>);
        impl std::io::Read for Output {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(0)
            }
        }
        struct Input(Arc<parking_lot::Mutex<Vec<u8>>>);
        impl std::io::Write for Input {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut app = TestAppContext::new();
        let (mut cx, _) = open_app(&mut app);
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let terminal = cx.update(|_, cx| {
            cx.new(|cx| {
                TerminalView::new(
                    Input(writes.clone()),
                    Output(reads.clone()),
                    TerminalConfig::default(),
                    cx,
                )
                .with_start_callback({
                    let starts = starts.clone();
                    move || {
                        starts.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
        });
        cx.run_until_parked();
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        for text in ["first command\r", "second command\r"] {
            let completion = cx.update(|_, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.release_idle_resources();
                    terminal.send_text_with_completion(text, cx).unwrap()
                })
            });
            cx.run_until_parked();
            smol::block_on(completion.wait()).unwrap();
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(&*writes.lock(), b"first command\rsecond command\r");
    }
}

#[cfg(test)]
#[path = "tests/resize_tests.rs"]
mod resize_tests;

#[cfg(test)]
mod xtgettcap_tests {
    //! End-to-end check of the terminal crate's XTGETTCAP passthrough: a
    //! capability query must produce a reply on the PTY writer and must never
    //! reach the grid as printable output.

    use alacritty_terminal::event::WindowSize;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use terminal::{ColorPalette, GpuiEventProxy, PtyWriter, TerminalNotifier, TerminalState};

    /// Writer sink that records everything the terminal writes back.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A terminal with a real PTY writer installed, like the app has.
    fn terminal_with_capture() -> (TerminalState, Capture) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let proxy = GpuiEventProxy::new(tx);
        // The notifier slot is shared with the proxy/Term, so installing the
        // writer after construction is exactly what `TerminalView` does.
        let slot = proxy.notifier_slot();
        let state = TerminalState::new(80, 24, proxy);

        let capture = Capture::default();
        let writer: PtyWriter = Arc::new(Mutex::new(
            Box::new(capture.clone()) as Box<dyn std::io::Write + Send>
        ));
        *slot.lock() = Some(TerminalNotifier::new(
            writer,
            Arc::new(Mutex::new(WindowSize {
                num_lines: 24,
                num_cols: 80,
                cell_width: 8,
                cell_height: 16,
            })),
            Arc::new(Mutex::new(ColorPalette::default())),
        ));

        (state, capture)
    }

    #[test]
    fn rgb_capability_is_answered_with_a_value() {
        let (mut state, capture) = terminal_with_capture();

        // "RGB" hex-encoded is `524742`.
        state.process_bytes(b"\x1bP+q524742\x1b\\");

        let reply = String::from_utf8_lossy(&capture.0.lock()).to_string();
        // `1` hex-encoded is `31`.
        assert_eq!(reply, "\x1bP1+r524742=31\x1b\\");
    }

    #[test]
    fn unknown_capability_gets_the_invalid_reply() {
        let (mut state, capture) = terminal_with_capture();

        // "XYZ" hex-encoded is `58595a`.
        state.process_bytes(b"\x1bP+q58595a\x1b\\");

        let reply = String::from_utf8_lossy(&capture.0.lock()).to_string();
        assert_eq!(reply, "\x1bP0+r58595a\x1b\\");
    }

    #[test]
    fn multiple_capabilities_get_one_reply_each() {
        let (mut state, capture) = terminal_with_capture();

        // "RGB;XYZ" -> `524742;58595a`
        state.process_bytes(b"\x1bP+q524742;58595a\x1b\\");

        let reply = String::from_utf8_lossy(&capture.0.lock()).to_string();
        assert_eq!(reply, "\x1bP1+r524742=31\x1b\\\x1bP0+r58595a\x1b\\");
    }

    #[test]
    fn query_is_not_printed_to_the_grid() {
        let (mut state, capture) = terminal_with_capture();

        state.process_bytes(b"before \x1bP+q524742\x1b\\ after\r\n");

        // Content starts on the top row, so read the whole visible screen.
        let screen = state.bottom_lines(usize::MAX).join("\n");
        assert!(
            screen.contains("before  after"),
            "query text leaked into the grid: {screen:?}"
        );
        assert!(
            !capture.0.lock().is_empty(),
            "the query should have been answered"
        );
    }
}

#[cfg(test)]
#[path = "tests/pane_tests.rs"]
mod pane_tests;

#[cfg(test)]
mod settings_and_theme_tests {
    use super::*;
    use gpui::{Modifiers, VisualContext, point};
    use resize_tests::{DrawFrame, TestAppContext, open_app};

    #[test]
    fn settings_back_button_returns_to_workspace() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.active_view = ActiveView::Settings;
            cx.notify();
        });
        cx.draw_frame().expect("draw settings page frame");

        let bounds = cx
            .debug_bounds("settings-close-btn")
            .expect("settings back button bounds");
        let center = point(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        cx.simulate_mouse_move(center, None, Modifiers::default());
        cx.simulate_click(center, Modifiers::default());

        assert_eq!(
            app_state.read_with(&cx, |state, _| state.active_view),
            ActiveView::Workspace,
            "clicking the back button must leave Settings"
        );
    }

    #[test]
    fn settings_page_toggle_and_render() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        // Initially on workspace
        let initial_view = app_state.read_with(&cx, |state, _| state.active_view);
        assert_eq!(initial_view, ActiveView::Workspace);

        // Switch to settings
        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.active_view = ActiveView::Settings;
            cx.notify();
        });
        cx.draw_frame().expect("draw settings page frame");
        let in_settings = app_state.read_with(&cx, |state, _| state.active_view);
        assert_eq!(in_settings, ActiveView::Settings);

        // Switch back to workspace
        cx.update_window_entity(&app_state, |state, _window, cx| {
            state.active_view = ActiveView::Workspace;
            cx.notify();
        });
        cx.draw_frame().expect("draw workspace page frame");
        let back_to_workspace = app_state.read_with(&cx, |state, _| state.active_view);
        assert_eq!(back_to_workspace, ActiveView::Workspace);
    }

    #[test]
    fn settings_keyboard_shortcuts() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        cx.draw_frame().unwrap();
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.focus_pane(1, window, cx);
        });
        let shortcut = if cfg!(target_os = "macos") { "cmd-," } else { "ctrl-," };
        for close in ["escape", shortcut] {
            cx.simulate_keystrokes(shortcut);
            cx.draw_frame().unwrap();
            assert_eq!(app_state.read_with(&cx, |s, _| s.active_view), ActiveView::Settings);

            cx.simulate_keystrokes(close);
            cx.draw_frame().unwrap();
            assert_eq!(app_state.read_with(&cx, |s, _| s.active_view), ActiveView::Workspace);
        }
    }

    #[test]
    fn terminal_pixels_follow_theme_after_returning_from_settings() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.split_pane(1, gpui::Axis::Horizontal, window, cx);
        });
        for (preference, light) in [
            (ThemePreference::Dark, false),
            (ThemePreference::Light, true),
            (ThemePreference::Dark, false),
            (ThemePreference::System, true),
        ] {
            cx.update_window_entity(&app_state, |state, window, cx| {
                state.active_view = ActiveView::Settings;
                state.set_theme_preference(preference, Some(window), cx);
            });
            cx.draw_frame().expect("draw settings");
            cx.update_window_entity(&app_state, |state, _, cx| {
                state.active_view = ActiveView::Workspace;
                cx.notify();
            });
            cx.draw_frame().expect("draw terminal");
            let bounds = app_state.read_with(&cx, |state, cx| {
                state.projects[0].sessions[0]
                    .panes
                    .iter()
                    .map(|pane| pane.terminal.read(cx).last_bounds())
                    .collect::<Vec<_>>()
            });
            assert_eq!(bounds.len(), 2);
            let pixels = cx.update(|window, _| {
                let image = window.render_to_image().expect("capture terminal");
                let scale = window.scale_factor();
                bounds
                    .iter()
                    .map(|bounds| {
                        let x = ((f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0)
                            * scale) as u32;
                        let y = ((f32::from(bounds.origin.y) + f32::from(bounds.size.height)
                            - 12.0)
                            * scale) as u32;
                        *image.get_pixel(x, y)
                    })
                    .collect::<Vec<_>>()
            });
            for pixel in pixels {
                if light {
                    assert!(
                        pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200,
                        "light terminal pixel: {pixel:?}"
                    );
                } else {
                    assert!(
                        pixel[0] < 60 && pixel[1] < 60 && pixel[2] < 60,
                        "dark terminal pixel: {pixel:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn session_or_pane_selection_resets_settings_view() {
        let mut cx = TestAppContext::new();
        let (mut cx, app_state) = open_app(&mut cx);

        // In settings, selecting session switches to workspace
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.active_view = ActiveView::Settings;
            state.select_session(0, 0, window, cx);
        });
        assert_eq!(
            app_state.read_with(&cx, |state, _| state.active_view),
            ActiveView::Workspace
        );

        // In settings, focusing pane switches to workspace
        cx.update_window_entity(&app_state, |state, window, cx| {
            state.active_view = ActiveView::Settings;
            state.focus_pane(1, window, cx);
        });
        assert_eq!(
            app_state.read_with(&cx, |state, _| state.active_view),
            ActiveView::Workspace
        );
    }
}

#[cfg(test)]
#[path = "tests/shob_bottom_input_tests.rs"]
mod shob_bottom_input_tests;
