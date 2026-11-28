impl AppState {
    fn render_terminal_pane(
        &self,
        pane: &TerminalPane,
        is_maximized: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let pane_id = pane.id;
        let is_focused = self.focused_pane_id == Some(pane_id);
        let tokens = self.tokens(cx);
        let preview_tab = if !pane.tool_panel
            || pane
                .diff_preview
                .as_ref()
                .is_some_and(|view| !view.file_only)
        {
            if let Some((view, file)) = pane
                .diff_preview
                .as_ref()
                .and_then(|view| view.file_view.as_ref().map(|file| (view, file)))
            {
                let deleted = matches!(file.state, ReadOnlyFileState::Ready { deleted: true, .. });
                Some(self.render_preview_tab(pane_id, &file.path, !view.file_only, deleted, cx))
            } else {
                pane.markdown_preview.as_ref().map(|(path, _)| {
                    self.render_preview_tab(pane_id, &path.to_string_lossy(), false, false, cx)
                })
            }
        } else {
            None
        };

        div()
            .id(("terminal-pane", pane_id))
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(tokens.pane_bg)
            .when(!pane.tool_panel, |view| {
                view.rounded(px(10.0))
                    .shadow(vec![gpui::BoxShadow {
                        color: gpui::Hsla {
                            h: 0.0,
                            s: 0.0,
                            l: 0.0,
                            a: 0.22,
                        },
                        blur_radius: px(8.0),
                        spread_radius: px(0.0),
                        offset: gpui::point(px(0.0), px(2.0)),
                        inset: false,
                    }])
                    .border(px(0.5))
                    .border_color(if is_focused {
                        tokens.pane_border_focused
                    } else {
                        tokens.pane_border
                    })
            })
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if this.pane_ref(pane_id).is_some_and(|p| {
                        p.chat.auth.is_some() || p.chat.active && !p.chat.requests.is_empty()
                    }) {
                        return;
                    }
                    this.focus_pane(pane_id, window, cx);
                }),
            )
            .children(if pane.tool_panel {
                preview_tab
            } else {
                Some(preview_tab.unwrap_or_else(|| {
                    div()
                        .h(px(28.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(8.0))
                        .bg(tokens.pane_header_bg)
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(tokens.pane_header_fg)
                                .child(pane.display_title.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .id(("split-v-btn", pane_id))
                                        .size(px(22.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.0))
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|s| s.bg(tokens.pane_header_btn_hover))
                                        .child(
                                            svg()
                                                .path("icons/split-vertical.svg")
                                                .size(px(13.0))
                                                .text_color(tokens.pane_header_icon)
                                                .hover(|s| {
                                                    s.text_color(tokens.pane_header_icon_hover)
                                                }),
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.split_pane(
                                                    pane_id,
                                                    gpui::Axis::Horizontal,
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        ),
                                )
                                .child(
                                    div()
                                        .id(("split-h-btn", pane_id))
                                        .size(px(22.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.0))
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|s| s.bg(tokens.pane_header_btn_hover))
                                        .child(
                                            svg()
                                                .path("icons/split-horizontal.svg")
                                                .size(px(13.0))
                                                .text_color(tokens.pane_header_icon)
                                                .hover(|s| {
                                                    s.text_color(tokens.pane_header_icon_hover)
                                                }),
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.split_pane(
                                                    pane_id,
                                                    gpui::Axis::Vertical,
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        ),
                                )
                                .child(
                                    div()
                                        .id(("maximize-btn", pane_id))
                                        .size(px(22.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.0))
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|s| s.bg(tokens.pane_header_btn_hover))
                                        .child(
                                            svg()
                                                .path(if is_maximized {
                                                    "icons/pane-restore.svg"
                                                } else {
                                                    "icons/pane-maximize.svg"
                                                })
                                                .size(px(13.0))
                                                .text_color(tokens.pane_header_icon)
                                                .hover(|s| {
                                                    s.text_color(tokens.pane_header_icon_hover)
                                                }),
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.toggle_maximize_pane(pane_id, cx);
                                            }),
                                        ),
                                )
                                .child(
                                    div()
                                        .id(("close-btn", pane_id))
                                        .size(px(22.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.0))
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|s| s.bg(tokens.pane_header_btn_hover))
                                        .child(
                                            svg()
                                                .path("icons/pane-close.svg")
                                                .size(px(13.0))
                                                .text_color(tokens.pane_header_icon)
                                                .hover(|s| s.text_color(rgba(0xf87171ff))),
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.close_pane(pane_id, window, cx);
                                            }),
                                        ),
                                ),
                        )
                }))
            })
            .child(
                div()
                    .flex_1()
                    .size_full()
                    .overflow_hidden()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.focus_pane(pane_id, window, cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            if this.pane_ref(pane_id).is_some_and(|pane| {
                                !pane.files_active
                                    && pane.diff_preview.is_none()
                                    && pane.markdown_preview.is_none()
                            }) {
                                cx.stop_propagation();
                                this.open_terminal_context_menu(
                                    pane_id,
                                    event.position,
                                    window,
                                    cx,
                                );
                            }
                        }),
                    )
                    .child(if pane.files_active {
                        div()
                            .size_full()
                            .capture_any_mouse_down(cx.listener(
                                move |this, event: &MouseDownEvent, window, cx| {
                                    if event.button == MouseButton::Left {
                                        this.focus_pane(pane_id, window, cx);
                                        cx.notify();
                                    }
                                },
                            ))
                            .child(pane.files_surface.as_ref().unwrap().0.clone())
                            .into_any_element()
                    } else if let Some(view) = &pane.diff_preview {
                        self.render_project_diff(pane_id, view, window, cx)
                    } else if let Some((_, html)) = &pane.markdown_preview {
                        div()
                            .id(("markdown-preview", pane_id))
                            .size_full()
                            .bg(cx.theme().editor_background())
                            .overflow_y_scrollbar()
                            .px(px(28.0))
                            .py(px(20.0))
                            .text_color(tokens.text_primary)
                            .child(TextView::html(
                                format!("markdown-preview-body-{pane_id}"),
                                html.clone(),
                                window,
                                cx,
                            ))
                            .into_any_element()
                    } else if pane.chat.active {
                        self.render_agent_chat(pane, window, cx).into_any_element()
                    } else {
                        pane.terminal.clone().into_any_element()
                    }),
            )
            .when(
                !pane.files_active
                    && pane.markdown_preview.is_none()
                    && pane.diff_preview.is_none()
                    && pane.should_show_bottom_input(cx)
                    && pane.chat.worker_view.is_none(),
                |this| this.child(self.render_shob_input_bar(pane, false, window, cx)),
            )
    }

    fn render_pane_node(
        &self,
        node: &PaneNode,
        session: &Session,
        is_maximized: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match node {
            PaneNode::Leaf(pane_id) => {
                if let Some(pane) = session
                    .panes
                    .iter()
                    .find(|p| p.id == *pane_id && !p.tool_panel && !p.one_mode)
                {
                    div()
                        .size_full()
                        .p(px(4.0))
                        .child(self.render_terminal_pane(pane, is_maximized, window, cx))
                        .into_any_element()
                } else {
                    div().into_any_element()
                }
            }
            PaneNode::Split {
                id,
                axis,
                left,
                right,
            } => {
                if !left.has_terminal_layout_pane(session) {
                    return self.render_pane_node(right, session, is_maximized, window, cx);
                }
                if !right.has_terminal_layout_pane(session) {
                    return self.render_pane_node(left, session, is_maximized, window, cx);
                }
                let left_el = self.render_pane_node(left, session, is_maximized, window, cx);
                let right_el = self.render_pane_node(right, session, is_maximized, window, cx);
                let is_horizontal = *axis == gpui::Axis::Horizontal;
                let split_id = ("terminal-split", *id);

                if is_horizontal {
                    h_resizable(split_id)
                        .child(
                            resizable_panel()
                                .show_handle_line(false)
                                .size_range(px(100.0)..px(3000.0))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .size_full()
                                        .child(div().flex_1().size_full().child(left_el)),
                                ),
                        )
                        .child(
                            resizable_panel()
                                .show_handle_line(false)
                                .size_range(px(100.0)..px(3000.0))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .size_full()
                                        .child(div().flex_1().size_full().child(right_el)),
                                ),
                        )
                        .into_any_element()
                } else {
                    v_resizable(split_id)
                        .child(
                            resizable_panel()
                                .show_handle_line(false)
                                .size_range(px(70.0)..px(3000.0))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .size_full()
                                        .child(div().flex_1().size_full().child(left_el)),
                                ),
                        )
                        .child(
                            resizable_panel()
                                .show_handle_line(false)
                                .size_range(px(70.0)..px(3000.0))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .size_full()
                                        .child(div().flex_1().size_full().child(right_el)),
                                ),
                        )
                        .into_any_element()
                }
            }
        }
    }

    /// The pane entity a top-level action (font size) applies to: the focused
    /// pane, or the first pane of the active session's screen.
    fn active_pane_terminal(&self) -> Option<Entity<TerminalView>> {
        let pane = self
            .focused_pane_id
            .and_then(|id| self.pane_ref(id))
            .or_else(|| self.active_session()?.panes.first())?;
        (!pane.is_browser()).then(|| pane.terminal.clone())
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;

        // The provider/model dialog is modal: no workspace shortcuts fire
        // while it is open. Escape is handled by the capture listener.
        if self.bot_form.is_some() || self.open_agent_auth_pane().is_some() {
            return;
        }

        let shortcut = if cfg!(target_os = "macos") {
            keystroke.modifiers.platform
        } else {
            keystroke.modifiers.control
        };
        if shortcut && !keystroke.modifiers.alt {
            match keystroke.key.as_str() {
                "n" => {
                    self.handle_sidebar_new_session(keystroke.modifiers.shift, window, cx);
                    cx.stop_propagation();
                    return;
                }
                "k" if !keystroke.modifiers.shift => {
                    self.open_sidebar_search(window, cx);
                    cx.stop_propagation();
                    return;
                }
                "b" if keystroke.modifiers.shift => {
                    self.open_sidebar_project_picker(SidebarProjectPickerMode::Scope, window, cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
        if self.focused_browser_pane(window, cx).is_some() {
            return;
        }

        if keystroke.modifiers.control && (keystroke.key == "+" || keystroke.key == "=") {
            if let Some(entity) = self.active_pane_terminal() {
                entity.update(cx, |terminal, cx| {
                    let mut config = terminal.config().clone();
                    config.font_size += px(1.0);
                    terminal.update_config(config, cx);
                });
            }
            cx.stop_propagation();
        } else if keystroke.modifiers.control && keystroke.key == "-" {
            if let Some(entity) = self.active_pane_terminal() {
                entity.update(cx, |terminal, cx| {
                    let mut config = terminal.config().clone();
                    if config.font_size > px(6.0) {
                        config.font_size -= px(1.0);
                        terminal.update_config(config, cx);
                    }
                });
            }
            cx.stop_propagation();
        }
    }

    // ---------------------------------------------------------------------
    // Sidebar: projects and their sessions
    // ---------------------------------------------------------------------

    fn sidebar_docked(visible: bool, window_width: Pixels) -> bool {
        visible && window_width >= COMPACT_WINDOW_WIDTH
    }

    fn sidebar_max_width(window_width: Pixels) -> Pixels {
        (window_width - SIDEBAR_MAIN_MIN_WIDTH).max(SIDEBAR_MIN_WIDTH)
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = window.viewport_size().width < COMPACT_WINDOW_WIDTH;
        let visible = if compact {
            self.compact_sidebar_open = !self.compact_sidebar_open;
            self.compact_sidebar_open
        } else {
            self.sidebar_visible = !self.sidebar_visible;
            self.schedule_workspace_save(cx);
            self.sidebar_visible
        };
        if !visible {
            self.close_sidebar_search(window, cx);
            self.close_sidebar_project_picker(window, cx);
            self.close_sidebar_thread_menu(cx);
        }
        cx.notify();
    }

    /// Move the sidebar edge to `width`, clamped to the allowed range.
    ///
    /// Returns whether the width actually changed, so callers can avoid a
    /// needless repaint while a drag is stationary.
    fn set_sidebar_width(&mut self, width: Pixels, window: &Window) -> bool {
        let max = Self::sidebar_max_width(window.viewport_size().width);
        let clamped = width.clamp(SIDEBAR_MIN_WIDTH, max);
        if clamped == self.sidebar_width {
            return false;
        }
        self.sidebar_width = clamped;
        true
    }

    /// The invisible strip along the sidebar's right border.
    ///
    /// Grabbing it records where the edge sits relative to the pointer; the
    /// actual movement is tracked by the root element, which spans the window
    /// and so keeps receiving moves even when the pointer runs past the strip.
    /// A hairline lights up on hover so the edge is discoverable.
    fn render_sidebar_resize_handle(&self, tokens: ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let active = self.sidebar_resizing;
        let handle = div()
            .id("sidebar-resize-handle")
            .debug_selector(|| "sidebar-resize-handle".to_string())
            .group("sidebar-resize")
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(SIDEBAR_RESIZE_HANDLE_WIDTH)
            .flex()
            .justify_end()
            .cursor_col_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if event.click_count == 2 {
                        this.sidebar_width = SIDEBAR_WIDTH;
                        this.schedule_workspace_save(cx);
                    } else {
                        let edge = this
                            .sidebar_width
                            .min(Self::sidebar_max_width(window.viewport_size().width));
                        this.sidebar_resize_offset = Some(event.position.x - edge);
                        this.sidebar_resizing = true;
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .h_full()
                    .w(px(2.0))
                    .when(active, |this| this.bg(tokens.ring))
                    .group_hover("sidebar-resize", |this| this.bg(tokens.ring)),
            );
        self.sidebar_tooltip(handle, "sidebar-resize-tooltip", "Resize sidebar", SidebarTooltipSide::Right, tokens, cx)
    }

    /// Follow the pointer while the sidebar edge is held.
    fn on_sidebar_resize_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sidebar_resizing {
            return;
        }
        // A release we never saw (e.g. the button went up outside the window)
        // ends the drag instead of leaving the edge glued to the pointer.
        if event.pressed_button != Some(MouseButton::Left) {
            self.finish_sidebar_resize(cx);
            return;
        }
        let Some(offset) = self.sidebar_resize_offset else {
            return;
        };
        let width = event.position.x - offset;
        let collapse = width < SIDEBAR_MIN_WIDTH * 0.25;
        let changed = self.sidebar_resize_collapse != collapse;
        self.sidebar_resize_collapse = collapse;
        if self.set_sidebar_width(width, window) || changed {
            cx.notify();
        }
    }

    /// End a sidebar resize drag, persisting the new width once.
    fn finish_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        if !self.sidebar_resizing {
            return;
        }
        self.sidebar_resizing = false;
        self.sidebar_resize_offset = None;
        if std::mem::take(&mut self.sidebar_resize_collapse) {
            self.sidebar_visible = false;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    /// Keep the search field mounted so clearing it preserves the header height.
    fn ensure_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_search.is_some() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscription = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                // Read the value after the input's update has released its lease.
                let weak = cx.entity().downgrade();
                cx.defer(move |cx| {
                    let _ = weak.update(cx, Self::update_sidebar_message_search);
                });
            }
        });
        self.sidebar_search = Some(SidebarSearch::new(input, subscription));
    }

    fn open_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_sidebar_search(window, cx);
        if window.viewport_size().width < COMPACT_WINDOW_WIDTH {
            self.compact_sidebar_open = true;
        } else if !self.sidebar_visible {
            self.sidebar_visible = true;
            self.schedule_workspace_save(cx);
        }
        self.sidebar_project_picker = None;
        self.sidebar_thread_menu = None;
        if let Some(search) = self.sidebar_search.as_mut() {
            if !search.input.read(cx).focus_handle().is_focused(window) {
                search.previous_focus = window.focused(cx);
            }
            search.input.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn clear_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self
            .sidebar_search
            .as_ref()
            .map(|search| search.input.clone());
        if let Some(input) = input {
            input.update(cx, |state, cx| state.set_value("", window, cx));
            cx.notify();
        }
    }

    fn close_sidebar_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.sidebar_search.as_ref().and_then(|search| {
            let input = search.input.read(cx);
            let focused = input.focus_handle().is_focused(window);
            if !focused && input.value().is_empty() {
                return None;
            }
            Some((search.input.clone(), search.previous_focus.clone(), focused))
        });
        if let Some((input, previous_focus, focused)) = snapshot {
            input.update(cx, |state, cx| state.set_value("", window, cx));
            if focused {
                if let Some(previous_focus) = previous_focus {
                    window.focus(&previous_focus, cx);
                } else {
                    window.blur(cx);
                }
            }
            cx.notify();
        }
    }

    fn sidebar_search_query(&self, cx: &App) -> String {
        self.sidebar_search
            .as_ref()
            .map(|search| search.input.read(cx).value().trim().to_string())
            .unwrap_or_default()
    }

    fn sidebar_search_handles_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let focused = self.sidebar_search_focused(window, cx);
        if !focused {
            return false;
        }
        if key == "escape" {
            cx.stop_propagation();
            self.close_sidebar_search(window, cx);
            return true;
        }
        if !matches!(key, "up" | "down" | "enter") {
            return true;
        }
        let query = self.sidebar_search_query(cx);
        if query.is_empty() {
            return true;
        }
        let rows = self.sidebar_search_rows(&query);
        cx.stop_propagation();
        if rows.is_empty() {
            return true;
        }
        let selected = self.sidebar_search.as_ref().and_then(|search| search.selected)
            .and_then(|id| rows.iter().position(|row| self.sidebar_thread_id(row.thread) == id));
        let last = rows.len() - 1;
        match key {
            "enter" => {
                let row = rows[selected.unwrap_or(0)].thread;
                self.select_session(row.project_idx, row.session_idx, window, cx);
            }
            _ => {
                let index = if key == "down" {
                    selected.map_or(0, |index| (index + 1).min(last))
                } else {
                    selected.map_or(last, |index| index.saturating_sub(1))
                };
                let id = self.sidebar_thread_id(rows[index].thread);
                if let Some(search) = self.sidebar_search.as_mut() {
                    search.selected = Some(id);
                    search.scroll.scroll_to_item(index);
                }
                cx.notify();
            }
        }
        true
    }

    fn sidebar_search_focused(&self, window: &Window, cx: &App) -> bool {
        self.sidebar_search
            .as_ref()
            .is_some_and(|search| search.input.read(cx).focus_handle().is_focused(window))
    }

    fn open_sidebar_project_picker(
        &mut self,
        mode: SidebarProjectPickerMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_thread_menu = None;
        let previous_focus = window.focused(cx);
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search projects"));
        let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                if let Some(picker) = this.sidebar_project_picker.as_mut()
                    && picker.mode == SidebarProjectPickerMode::NewSession
                {
                    picker.selected = 0;
                    picker.scroll.scroll_to_item(0);
                }
                cx.notify();
            }
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.sidebar_project_picker = Some(SidebarProjectPicker {
            mode,
            selected: 0,
            scroll: gpui::ScrollHandle::new(),
            input,
            previous_focus,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn close_sidebar_project_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.sidebar_project_picker.as_ref().map(|picker| {
            (
                picker.input.clone(),
                picker.previous_focus.clone(),
                picker.mode == SidebarProjectPickerMode::NewSession
                    || picker.input.read(cx).focus_handle().is_focused(window),
            )
        });
        self.sidebar_project_picker = None;
        if let Some((_, previous_focus, focused)) = snapshot
            && focused
            && let Some(previous_focus) = previous_focus
        {
            window.focus(&previous_focus, cx);
        }
        cx.notify();
    }

    fn sidebar_project_picker_query(&self, cx: &App) -> String {
        self.sidebar_project_picker
            .as_ref()
            .map(|picker| picker.input.read(cx).value().trim().to_lowercase())
            .unwrap_or_default()
    }

    fn new_session_project_matches<'a>(
        &'a self,
        query: &'a str,
    ) -> impl Iterator<Item = NewSessionTarget> + 'a {
        let mut projects = self.projects.iter().enumerate().filter_map(|(index, project)| {
            (!project.projectless
                && (sidebar_matches(query, project.name.as_str())
                    || sidebar_matches(query, &project.path.to_string_lossy())))
            .then_some(index)
        });
        projects
            .next()
            .into_iter()
            .map(NewSessionTarget::Project)
            .chain(sidebar_matches(query, NO_PROJECT_LABEL).then_some(NewSessionTarget::NoProject))
            .chain(sidebar_matches(query, CREATE_BOT_LABEL).then_some(NewSessionTarget::CreateBot))
            .chain(projects.map(NewSessionTarget::Project))
    }

    fn sidebar_project_picker_focused(&self, window: &Window, cx: &App) -> bool {
        self.sidebar_project_picker
            .as_ref()
            .is_some_and(|picker| picker.input.read(cx).focus_handle().is_focused(window))
    }

    fn close_sidebar_thread_menu(&mut self, cx: &mut Context<Self>) {
        self.sidebar_custom_snooze = None;
        let closed = self.sidebar_thread_menu.take().is_some() | self.sidebar_folder_menu.take().is_some();
        if closed {
            cx.notify();
        }
    }

    fn sidebar_scope_indices(&self) -> Vec<usize> {
        let sidebar = &self.settings.sidebar;
        self.projects
            .iter()
            .enumerate()
            .filter(|(_, project)| {
                sidebar.project_scope.as_ref() == Some(&project.path)
                    || sidebar.extra_project_scopes.contains(&project.path)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn sidebar_visible_project_indices(&self) -> Vec<usize> {
        let scoped = self.sidebar_scope_indices();
        if scoped.is_empty() {
            (0..self.projects.len()).collect()
        } else {
            scoped
        }
    }

    fn set_sidebar_project_scope(
        &mut self,
        project_idx: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.sidebar.project_scope = project_idx
            .and_then(|idx| self.projects.get(idx))
            .map(|project| project.path.clone());
        self.settings.sidebar.extra_project_scopes.clear();
        self.settled_visible_count = SIDEBAR_SETTLED_INITIAL_COUNT;
        self.close_sidebar_project_picker(window, cx);
        self.schedule_workspace_save(cx);
    }

    fn toggle_sidebar_project_scope(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(project) = self.projects.get(index) else {
            return;
        };
        let sidebar = &mut self.settings.sidebar;
        if sidebar.project_scope.as_ref() == Some(&project.path) {
            sidebar.project_scope = sidebar.extra_project_scopes.pop();
        } else if let Some(position) = sidebar.extra_project_scopes.iter().position(|path| path == &project.path) {
            sidebar.extra_project_scopes.remove(position);
        } else if sidebar.project_scope.is_none() {
            sidebar.project_scope = Some(project.path.clone());
        } else {
            sidebar.extra_project_scopes.push(project.path.clone());
        }
        self.settled_visible_count = SIDEBAR_SETTLED_INITIAL_COUNT;
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn choose_new_session_target(
        &mut self,
        target: NewSessionTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            NewSessionTarget::Project(index) => {
                self.choose_new_session_project(Some(index), window, cx);
            }
            NewSessionTarget::NoProject => self.choose_new_session_project(None, window, cx),
            NewSessionTarget::CreateBot => {
                self.close_sidebar_project_picker(window, cx);
                self.open_bot_form(None, window, cx);
            }
        }
    }

    fn choose_new_session_project(
        &mut self,
        project_idx: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project_idx = project_idx
            .or_else(|| self.projects.iter().position(|project| project.projectless));
        let scope = self.sidebar_scope_indices();
        if !scope.is_empty() && project_idx.is_none_or(|index| !scope.contains(&index)) {
            self.settings.sidebar.project_scope = None;
            self.settings.sidebar.extra_project_scopes.clear();
        }
        self.create_new_sidebar_session(project_idx, window, cx);
    }

    fn toggle_sidebar_shelf(&mut self, shelf: SidebarShelf, cx: &mut Context<Self>) {
        match shelf {
            SidebarShelf::Snoozed => {
                self.settings.sidebar.snoozed_expanded = !self.settings.sidebar.snoozed_expanded;
            }
            SidebarShelf::Settled => {
                self.settings.sidebar.settled_expanded = !self.settings.sidebar.settled_expanded;
            }
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }
}
