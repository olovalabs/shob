impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let inset = window_chrome::titlebar_inset(window);
        let content = self.render_app(window, cx);
        if inset > px(0.0) {
            div()
                .size_full()
                .pt(inset)
                .bg(self.tokens(cx).titlebar_bg)
                .child(content)
                .into_any_element()
        } else {
            content
        }
    }
}

impl AppState {
    fn render_app(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        #[cfg(test)]
        APP_ROOT_RENDERS.with(|count| count.set(count.get() + 1));
        // The wrapper marks only panes mounted by this root pass. Background
        // sessions and panes hidden by previews/maximization stay dormant.
        let workspace = self.active_view == ActiveView::Workspace;
        let selected = if workspace { self.active_session().map(|session| session.id) } else { None };
        let seen_at = std::time::Instant::now();
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    if Some(session.id) == selected {
                        pane.idle_resources_released = false;
                        // Rendering is the strongest evidence the person is
                        // looking at this pane, and it records `last_seen` so a
                        // pane that was viewed once may later be hibernated.
                        pane.lifecycle.mark_seen(seen_at);
                    }
                    pane.chat.view_visible.set(false);
                }
            }
        }
        static FIRST_RENDER: std::sync::Once = std::sync::Once::new();
        FIRST_RENDER.call_once(|| startup_mark("first root render started"));
        if !self.startup_loaded {
            return div()
                .id("startup-splash")
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(self.tokens(cx).window_bg)
                .child(gpui::img("shob.png").w(px(96.0)).h(px(96.0)))
                .into_any_element();
        }
        if let Some(id) = self.active_session().map(|session| session.id)
            && self.navigation_history.get(self.navigation_position) != Some(&id)
        {
            self.navigation_history
                .truncate(self.navigation_position.saturating_add(1));
            if self.navigation_history.len() == 100 {
                self.navigation_history.remove(0);
            }
            self.navigation_history.push(id);
            self.navigation_position = self.navigation_history.len() - 1;
        }
        let diag_render = diagnostics::render_start();
        // "Looking" needs an active window: a visible but backgrounded window
        // has not been seen, so a finish there must still become Done.
        self.window_active = _window.is_window_active();
        let compact = _window.viewport_size().width < COMPACT_WINDOW_WIDTH;
        if !compact {
            self.compact_sidebar_open = false;
        }
        let sidebar_docked =
            Self::sidebar_docked(self.sidebar_visible, _window.viewport_size().width);
        if workspace {
            if !sidebar_docked && !self.compact_sidebar_open {
                self.close_sidebar_search(_window, cx);
            }
            self.ensure_panes_input_state(_window, cx);
        }

        let tokens = self.tokens(cx);
        // Track which pane owns keyboard focus each frame; the poller uses
        // this for Active-state resolution and for clearing sticky Done/Error.
        let mut focused_pane = None;
        if workspace && let Some(session) = self.active_session() {
            for pane in &session.panes {
                let is_term_focused =
                    !pane.chat.active && pane.terminal.read(cx).focus_handle().is_focused(_window);
                let is_input_focused = pane
                    .input_state
                    .as_ref()
                    .is_some_and(|input| input.read(cx).focus_handle().is_focused(_window));
                if is_term_focused || is_input_focused {
                    focused_pane = Some(pane.id);
                    break;
                }
            }
        }
        if let Some(focused_pane) = focused_pane {
            self.focused_pane_id = Some(focused_pane);
        } else if self.focused_pane_id.is_none() {
            self.focused_pane_id = self
                .active_session()
                .and_then(|s| s.panes.first())
                .map(|p| p.id);
        }

        // Drain alerts here, where a window exists to take notifications and
        // sounds; the poller has none. A blocked transition while looking only
        // rings (no toast), and a missed finish rings only when it happened.
        let alerts = std::mem::take(&mut self.pending_alerts);
        if !alerts.is_empty() {
            let weak = cx.entity().downgrade();
            _window.defer(cx, move |window, cx| {
                for alert in alerts {
                    crate::sound::play(alert.kind);
                    if alert.kind == attention::AlertKind::Blocked && alert.looking {
                        continue;
                    }
                    let (title, kind) = match alert.kind {
                        attention::AlertKind::Blocked => {
                            (format!("{} needs you", alert.name), NotificationType::Error)
                        }
                        attention::AlertKind::Done => {
                            (format!("{} finished", alert.name), NotificationType::Info)
                        }
                    };
                    let weak = weak.clone();
                    let pane_id = alert.pane_id;
                    let note = Notification::new()
                        .title(title)
                        .message(alert.place)
                        .with_type(kind)
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.focus_pane(pane_id, window, cx);
                            });
                        });
                    window.push_notification(note, cx);
                }
            });
        }

        if !workspace {
            let settings = self.render_settings_page(_window, cx);
            diagnostics::render_end(diag_render);
            return settings.into_any_element();
        }

        let terminal_area = if let Some(session) = self.active_session() {
            // Each session is its own screen: its pane tree (and maximized
            // pane) is rendered exclusively; other sessions' panes keep
            // running in the background.
            if self.settings.agent_layout {
                if let Some(pane) = self.one_mode_pane(session) {
                    if pane.chat.active
                        && pane.diff_preview.is_none()
                        && pane.markdown_preview.is_none()
                    {
                        let usage = pane.chat.usage.filter(|usage| {
                            usage.input != 0
                                || usage.output != 0
                                || usage.cache_read != 0
                                || usage.cache_write != 0
                                || usage.cost.is_some_and(|cost| cost > 0.0)
                        });
                        let usage_detail = pane.chat.usage_detail.clone();
                        let workspace_width = _window.viewport_size().width
                            - if sidebar_docked {
                                self.sidebar_width
                            } else {
                                px(0.0)
                            };
                        let wide = workspace_width >= px(1360.0) && !self.tool_panel_is_open(pane);
                        let info_visible = if wide {
                            pane.chat.info_open.unwrap_or(!pane.chat.visible_messages().is_empty() || pane.chat.worker_view.is_some())
                        } else {
                            pane.chat.info_open_compact
                        };
                        let ascii_welcome = self.palette_scheme == PaletteScheme::MidnightGarden
                            && tokens.is_dark
                            && pane.chat.visible_messages().is_empty()
                            && pane.chat.worker_view.is_none();
                        let chat = div()
                            .relative()
                            .size_full()
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .items_center()
                            .bg(tokens.chat().background)
                            .font_family(cx.theme().font_family.clone())
                            .child(self.render_conversation_header(pane.id, wide, info_visible, tokens, cx))
                            .when(ascii_welcome, |view| view.child(ascii_glow::render()))
                            .when(usage.is_some(), |view| {
                                view.child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .justify_end()
                                        .px(px(20.0))
                                        .pt(px(8.0))
                                        .child(
                                            div()
                                                .id(("agent-usage", pane.id))
                                                .debug_selector(|| "agent-usage".into())
                                                .text_size(px(10.0))
                                                .text_color(tokens.text_muted)
                                                .child(pane.chat.usage_label.clone())
                                                .tooltip(move |window, cx| {
                                                    gpui_component::tooltip::Tooltip::new(
                                                        usage_detail.clone(),
                                                    )
                                                    .build(window, cx)
                                                }),
                                        ),
                                )
                            })
                            .when(
                                pane.chat.visible_messages().is_empty()
                                    && pane.chat.worker_view.is_none(),
                                |view| {
                                    view.child(
                                        div()
                                            .w_full()
                                            .flex_1()
                                            .min_h_0()
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .justify_center()
                                            .gap(px(24.0))
                                            .child(self.render_agent_chat(pane, _window, cx))
                                            .child(
                                                div()
                                                    .w_full()
                                                    .max_w(if ascii_welcome {
                                                        px(688.0)
                                                    } else {
                                                        agent_appearance::COLUMN_WIDTH
                                                    })
                                                    .child(self.render_shob_input_bar(
                                                        pane, true, _window, cx,
                                                    )),
                                            ),
                                    )
                                },
                            )
                            .when(
                                !pane.chat.visible_messages().is_empty()
                                    || pane.chat.worker_view.is_some(),
                                |view| {
                                    view.child(
                                        div()
                                            .w_full()
                                            .flex_1()
                                            .min_h_0()
                                            .child(self.render_agent_chat(pane, _window, cx)),
                                    )
                                    .when(
                                        pane.chat.worker_view.is_none(),
                                        |view| {
                                            view.child(
                                                div()
                                                    .w_full()
                                                    .max_w(agent_appearance::COLUMN_WIDTH)
                                                    .child(self.render_shob_input_bar(
                                                        pane, false, _window, cx,
                                                    )),
                                            )
                                        },
                                    )
                                },
                            )
                            .when(
                                info_visible
                                    && (!pane.chat.visible_messages().is_empty()
                                        || pane.chat.worker_view.is_some()
                                        || pane.chat.info_open == Some(true)
                                        || pane.chat.info_open_compact),
                                |view| {
                                    view.child(self.render_focused_agent_info(
                                        pane,
                                        usage.is_some(),
                                        _window,
                                        cx,
                                    ))
                                },
                            )
                            .when(
                                pane.chat.worker_view.is_none()
                                    && !pane.chat.visible_messages().is_empty(),
                                |view| {
                                    let transcript = pane.chat.view.borrow();
                                    let Some(transcript) = transcript.as_ref() else {
                                        return view;
                                    };
                                    let mut scrollbar = pane.chat.focused_scrollbar.borrow_mut();
                                    let scrollbar = scrollbar.get_or_insert_with(|| {
                                        cx.new(|_| FocusedChatScrollbar {
                                            pane_id: pane.id,
                                            transcript_view: transcript.entity_id(),
                                            list: pane.chat.list_state.clone(),
                                            height: Rc::new(std::cell::Cell::new(px(0.0))),
                                        })
                                    });
                                    view.child(
                                        scrollbar.clone().cached(
                                            StyleRefinement::default().absolute().size_full(),
                                        ),
                                    )
                                },
                            )
                            .into_any_element();
                        if self.tool_panel_is_open(pane) {
                            let panel = self.active_tool_panel(pane);
                            let panel_owner = pane.id;
                            let width = (workspace_width - px(8.0)).max(px(200.0));
                            let ratio = pane
                                .tool_panel_chat_ratio
                                .or_else(|| {
                                    self.tool_panel_panes(panel_owner)
                                        .next()
                                        .and_then(|pane| pane.tool_panel_chat_ratio)
                                })
                                .unwrap_or(0.5);
                            let owner = cx.entity().downgrade();
                            h_resizable(("chat-tool-panel", panel_owner))
                                .child(
                                    resizable_panel()
                                        .size(width * ratio)
                                        .size_range(px(100.0)..px(10000.0))
                                        .child(div().size_full().overflow_hidden().child(chat)),
                                )
                                .child(
                                    resizable_panel()
                                        .size(width * (1.0 - ratio))
                                        .size_range(px(100.0)..px(10000.0))
                                        .child(
                                            div()
                                                .size_full()
                                                .flex()
                                                .flex_col()
                                                .overflow_hidden()
                                                .child(self.render_tool_panel_tabs(
                                                    pane,
                                                    panel.map(|tab| tab.id),
                                                    cx,
                                                ))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_h_0()
                                                        .overflow_hidden()
                                                        .children(
                                                            if let Some(focus) =
                                                                &pane.tool_panel_chooser
                                                            {
                                                                Some(
                                                                    self.render_tool_panel_chooser(
                                                                        panel_owner,
                                                                        focus,
                                                                        cx,
                                                                    )
                                                                    .into_any_element(),
                                                                )
                                                            } else {
                                                                panel.map(|tab| {
                                                                    self.render_terminal_pane(
                                                                        tab, false, _window, cx,
                                                                    )
                                                                    .into_any_element()
                                                                })
                                                            },
                                                        ),
                                                ),
                                        ),
                                )
                                .on_resize(move |state, _, cx| {
                                    let sizes = state.read(cx).sizes();
                                    if sizes.len() != 2 || sizes[0] + sizes[1] <= px(0.0) {
                                        return;
                                    }
                                    let ratio = (sizes[0] / (sizes[0] + sizes[1])).clamp(0.0, 1.0);
                                    let _ = owner.update(cx, |this, cx| {
                                        if let Some(panel) = this.pane_mut(panel_owner) {
                                            panel.tool_panel_chat_ratio = Some(ratio);
                                        }
                                        this.schedule_workspace_save(cx);
                                    });
                                })
                                .into_any_element()
                        } else {
                            chat
                        }
                    } else {
                        self.render_terminal_pane(pane, true, _window, cx)
                            .into_any_element()
                    }
                } else {
                    div().into_any_element()
                }
            } else if !session
                .panes
                .iter()
                .any(|pane| !pane.one_mode && !pane.tool_panel)
            {
                self.render_empty_terminal(cx).into_any_element()
            } else if let Some(max_pane_id) = session.maximized_pane_id {
                if let Some(pane) = session
                    .panes
                    .iter()
                    .find(|p| p.id == max_pane_id && !p.tool_panel && !p.one_mode)
                {
                    div()
                        .size_full()
                        .p(px(4.0))
                        .child(self.render_terminal_pane(pane, true, _window, cx))
                        .into_any_element()
                } else if let Some(tree) = &session.pane_tree {
                    self.render_pane_node(tree, session, false, _window, cx)
                } else {
                    div().into_any_element()
                }
            } else if let Some(tree) = &session.pane_tree {
                self.render_pane_node(tree, session, false, _window, cx)
            } else if let Some(pane) = session.panes.first() {
                div()
                    .size_full()
                    .p(px(4.0))
                    .child(self.render_terminal_pane(pane, false, _window, cx))
                    .into_any_element()
            } else {
                div().into_any_element()
            }
        } else {
            self.render_empty_terminal(cx).into_any_element()
        };
        let agent_pane = self.focused_conversation_pane();
        let titlebar_status = self.active_session().map(|s| s.status)
            .filter(|status| *status != SessionStatus::Active);
        let panel_source = agent_pane
            .filter(|pane| !self.tool_panel_is_open(pane))
            .map(|pane| pane.id);

        let terminal_content = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(tokens.window_bg)
            .child(
                self.desktop_titlebar(cx)
                    .h(px(38.0))
                    .bg(tokens.titlebar_bg)
                    .border_color(tokens.titlebar_border)
                    .when(cfg!(target_os = "macos"), |bar| bar.pl_3())
                    .child(
                        div()
                            .flex()
                            .min_w_0()
                            .overflow_hidden()
                            .flex_1()
                            .items_center()
                            .gap_2()
                            .when(!sidebar_docked, |row| row.pl(px(112.0)))
                            .text_size(px(12.5))
                            .text_color(tokens.titlebar_fg)
                            .when(agent_pane.is_none(), |row| {
                                let project = self.active_project().map_or("No project", |p| p.name.as_str());
                                let session = self.active_session();
                                let title = session.map_or("No session", |s| s.title.as_str());
                                row.children(session.map(|s| {
                                    div()
                                        .size(px(16.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .flex_shrink_0()
                                        .child(session_logo_icon(s.logo.clone(), tokens.titlebar_fg))
                                }))
                                .child(div().min_w_0().truncate().child(format!("{project} — {title}")))
                            })
                            .children(titlebar_status.map(|status| {
                                let (dot_color, text_color, badge_bg) = status.colors();
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(rgba(badge_bg))
                                    .text_color(rgba(text_color))
                                    .child(div().size(px(6.0)).rounded_full().bg(rgba(dot_color)))
                                    .child(status.label())
                            }))
                            .when_some(panel_source, |row, pane_id| {
                                row.child(div().flex_1()).child(
                                    div()
                                        .id(("open-tool-panel", pane_id))
                                        .debug_selector(|| "open-tool-panel".into())
                                        .occlude()
                                        .size(px(28.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(5.0))
                                        .cursor_pointer()
                                        .hover(|style| style.bg(tokens.titlebar_btn_hover))
                                        .child(
                                            svg()
                                                .path("icons/panel-right.svg")
                                                .size(px(15.0))
                                                .text_color(tokens.titlebar_fg),
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.open_tool_panel(pane_id, window, cx);
                                            }),
                                        ),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .size_full()
                    .bg(tokens.window_bg)
                    .overflow_hidden()
                    .p(px(4.0))
                    .child(div().flex().flex_col().size_full().child(terminal_area)),
            );

        let root = div()
            .id("app-root")
            .relative()
            .flex()
            .size_full()
            .bg(tokens.window_bg)
            .text_color(tokens.text_primary)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_move(cx.listener(Self::on_sidebar_resize_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_sidebar_resize(cx)),
            )
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let keystroke = &event.keystroke;

                if keystroke.key == "escape" && this.system_monitor_open {
                    this.system_monitor_open = false;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }

                if keystroke.key == "escape"
                    && (this.sidebar_project_picker_focused(window, cx)
                        || this.sidebar_thread_menu.is_some())
                {
                    this.close_sidebar_project_picker(window, cx);
                    this.close_sidebar_thread_menu(cx);
                    cx.stop_propagation();
                    return;
                }
                if this
                    .sidebar_project_picker
                    .as_ref()
                    .is_some_and(|picker| picker.mode == SidebarProjectPickerMode::NewSession)
                {
                    let query = this.sidebar_project_picker_query(cx);
                    let matches: Vec<usize> = this
                        .projects
                        .iter()
                        .enumerate()
                        .filter(|(_, project)| {
                            sidebar_matches(&query, project.name.as_str())
                                || sidebar_matches(&query, &project.path.to_string_lossy())
                        })
                        .map(|(index, _)| index)
                        .collect();
                    match keystroke.key.as_str() {
                        "up" | "down" => {
                            if let Some(picker) = this.sidebar_project_picker.as_mut() {
                                picker.selected = if matches.is_empty() {
                                    0
                                } else if keystroke.key == "down" {
                                    (picker.selected + 1).min(matches.len() - 1)
                                } else {
                                    picker.selected.min(matches.len() - 1).saturating_sub(1)
                                };
                            }
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                        "enter" => {
                            if let Some(index) = this
                                .sidebar_project_picker
                                .as_ref()
                                .and_then(|picker| {
                                    matches
                                        .get(picker.selected.min(matches.len().saturating_sub(1)))
                                })
                                .copied()
                            {
                                this.choose_new_session_project(Some(index), window, cx);
                            }
                            cx.stop_propagation();
                            return;
                        }
                        _ => return,
                    }
                }
                let shortcut = if cfg!(target_os = "macos") {
                    keystroke.modifiers.platform
                } else {
                    keystroke.modifiers.control
                };
                if (shortcut || keystroke.modifiers.control)
                    && !keystroke.modifiers.alt
                    && keystroke.key == ","
                    && this.open_agent_auth_pane().is_none()
                {
                    this.open_settings(window, cx);
                    cx.stop_propagation();
                    return;
                }
                if shortcut && !keystroke.modifiers.alt {
                    match keystroke.key.as_str() {
                        "n" => {
                            this.handle_sidebar_new_session(keystroke.modifiers.shift, window, cx);
                            cx.stop_propagation();
                            return;
                        }
                        "k" if !keystroke.modifiers.shift => {
                            this.open_sidebar_search(window, cx);
                            cx.stop_propagation();
                            return;
                        }
                        "b" if keystroke.modifiers.shift => {
                            this.open_sidebar_project_picker(
                                SidebarProjectPickerMode::Scope,
                                window,
                                cx,
                            );
                            cx.stop_propagation();
                            return;
                        }
                        _ => {}
                    }
                }
                if this.sidebar_search_handles_key(&keystroke.key, window, cx) {
                    return;
                }

                if this.compact_sidebar_open && keystroke.key == "escape" {
                    this.compact_sidebar_open = false;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if this.image_viewer.is_some() {
                    if keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.image_viewer = None;
                        cx.notify();
                    }
                    return;
                }
                if let Some(pane_id) = this.open_agent_auth_pane() {
                    if keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.close_agent_auth(pane_id, window, cx);
                    }
                    return;
                }

                if this.active_view != ActiveView::Workspace
                    || this.renaming_project.is_some()
                    || this.history_search.is_some()
                {
                    return;
                }

                // Intercept paste anywhere in the workspace — the input bar
                // and the terminal grid — so clipboard images (or copied
                // image paths) attach instead of pasting text. Ctrl+V,
                // Cmd+V and Ctrl+Shift+V all count; plain text falls
                // through to the focused editor's own paste handler.
                let key = keystroke.key.as_str();
                let is_paste = ((key == "v" || key == "V")
                    && (keystroke.modifiers.control || keystroke.modifiers.platform)
                    && !keystroke.modifiers.alt)
                    || (key == "insert" && keystroke.modifiers.shift);
                if !is_paste {
                    return;
                }

                let Some(pane_id) = this.focused_pane_id else {
                    return;
                };
                // Full-screen programs (vim, less, htop) own the keyboard;
                // don't turn their Ctrl+V into an attachment.
                if this
                    .pane_ref(pane_id)
                    .is_some_and(|pane| pane.terminal.read(cx).is_alt_screen())
                {
                    return;
                }
                if this.attach_image_from_clipboard(pane_id, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .can_drop(|value, _, _| {
                value
                    .downcast_ref::<ExternalPaths>()
                    .is_some_and(|paths| paths.paths().iter().any(|path| path.is_file()))
            })
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.drop_external_files(paths, window, cx);
            }))
            .when(sidebar_docked, |this| {
                // Clamp on the way out too: a window that shrank since the
                // width was chosen must not leave the rail eating the terminal.
                let width = self
                    .sidebar_width
                    .min(Self::sidebar_max_width(_window.viewport_size().width));
                this.child(
                    div()
                        .w(width)
                        .h_full()
                        .flex_shrink_0()
                        .relative()
                        .child(self.render_sidebar(_window, cx))
                        .child(self.render_sidebar_resize_handle(cx)),
                )
            })
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .overflow_hidden()
                    .child(terminal_content),
            )
            .when(!sidebar_docked && !self.compact_sidebar_open, |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .h(px(48.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(sidebar_icon_button(
                            "terminal-sidebar-toggle-btn",
                            "icons/panel-left.svg",
                            16.0,
                            &tokens,
                            cx.listener::<MouseDownEvent>(|this, _, window, cx| {
                                if window.viewport_size().width < COMPACT_WINDOW_WIDTH {
                                    this.compact_sidebar_open = true;
                                } else {
                                    this.sidebar_visible = true;
                                    this.schedule_workspace_save(cx);
                                }
                                cx.notify();
                            }),
                        ))
                        .child(self.render_navigation_buttons(cx)),
                )
            })
            .when(compact && self.compact_sidebar_open, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(rgba(0x00000080))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.close_sidebar_search(window, cx);
                                this.compact_sidebar_open = false;
                                cx.notify();
                            }),
                        )
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .h_full()
                                .w(self
                                    .sidebar_width
                                    .min((_window.viewport_size().width - px(40.0)).max(px(0.0))))
                                .occlude()
                                .child(self.render_sidebar(_window, cx)),
                        ),
                )
            })
            .when(
                self.sidebar_project_picker
                    .as_ref()
                    .is_some_and(|picker| picker.mode == SidebarProjectPickerMode::NewSession),
                |this| {
                    this.child(self.render_new_session_project_modal(
                        self.sidebar_project_picker.as_ref().unwrap(),
                        cx,
                    ))
                },
            )
            .when(self.terminal_context_menu.is_some(), |this| {
                this.child(self.render_terminal_context_menu_layer(cx))
            })
            .when(self.history_search.is_some(), |this| {
                this.child(self.render_history_search_layer(cx))
            })
            .when(self.image_viewer.is_some(), |this| {
                this.child(self.render_image_viewer_layer(cx))
            })
            .when(self.open_agent_auth_pane().is_some(), |this| {
                this.child(self.render_agent_auth_layer(_window, cx))
            });
        diagnostics::render_end(diag_render);
        root.into_any_element()
    }
}

/// How many non-empty screen lines `T3_STATUS_DEBUG` prints per terminal.
const DEBUG_SCREEN_LINES: usize = 14;

/// Sidebar form of a session title: full text up to 160 chars, then an ellipsis.
/// Uses `char_indices().nth(160)` so short titles cost one pass that stops at
/// the end — unlike `chars().count()`, which always scans the whole string —
/// and is cached on the session so rendering never recomputes it.
fn truncate_title(title: &SharedString) -> SharedString {
    match title.as_str().char_indices().nth(160) {
        Some((idx, _)) => SharedString::from(format!("{}...", &title.as_str()[..idx])),
        None => title.clone(),
    }
}

/// SVG icons follow the current theme; raster agent logos keep their own colors.
fn session_logo_icon(logo: SharedString, color: Hsla) -> gpui::AnyElement {
    if logo.ends_with(".svg") {
        svg()
            .path(logo)
            .size(px(14.0))
            .text_color(color)
            .into_any_element()
    } else {
        gpui::img(logo)
            .size(px(14.0))
            .rounded(px(2.0))
            .into_any_element()
    }
}

/// Cheap, stable fingerprint of what is on screen, used to tell whether the
/// user has already looked at a finished/failed terminal.
fn screen_fingerprint(lines: &[String], title: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    lines.hash(&mut hasher);
    title.hash(&mut hasher);
    hasher.finish()
}

/// How alerts and the Agents list name a pane: its agent app first, else a
/// meaningful title, else a neutral fallback.
fn pane_agent_name(pane: &TerminalPane) -> String {
    if let Some(app) = &pane.app {
        return app.to_string();
    }
    let title = pane.display_title.trim();
    if !title.is_empty() && !title.starts_with("terminal ") {
        return title.to_string();
    }
    "An agent".to_string()
}
