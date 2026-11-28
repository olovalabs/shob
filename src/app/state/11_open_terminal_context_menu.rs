impl AppState {
    fn open_terminal_context_menu(
        &mut self,
        pane_id: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal_context_menu = Some(TerminalContextMenu { pane_id, position });
        self.focus_pane(pane_id, window, cx);
        cx.notify();
    }

    fn close_terminal_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.terminal_context_menu.is_some() {
            self.terminal_context_menu = None;
            cx.notify();
        }
    }

    fn copy_terminal_selection(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id)
            && let Some(pane) = self
                .projects
                .get(p_idx)
                .and_then(|p| p.sessions.get(s_idx))
                .and_then(|s| s.panes.get(pane_idx))
            && let Some(text) = pane.terminal.read(cx).selected_text()
            && !text.is_empty()
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            pane.terminal.update(cx, |term, cx| {
                term.clear_selection(cx);
            });
        }
        self.close_terminal_context_menu(cx);
        self.focus_pane(pane_id, window, cx);
    }

    fn paste_from_clipboard_into_pane(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = cx.read_from_clipboard() {
            // Clipboard images and copied image paths become attachments
            // instead of pasting either a path or nothing.
            if self.attach_images_from_item(pane_id, &item, window, cx) {
                self.close_terminal_context_menu(cx);
                return;
            }

            if let Some(text) = item.text() {
                let clean = text
                    .trim_matches(&['\r', '\n'][..])
                    .replace("\r\n", " ")
                    .replace('\n', " ")
                    .replace('\r', "");
                if !clean.is_empty() {
                    let Some(pane) = self.pane_ref(pane_id) else {
                        return;
                    };
                    if pane.should_show_bottom_input(cx) {
                        if let Some(input_state) = &pane.input_state {
                            input_state.update(cx, |state, cx| {
                                let cur = state.value().to_string();
                                state.set_value(format!("{cur}{clean}"), window, cx);
                                state.focus(window, cx);
                            });
                        }
                    } else {
                        pane.terminal.update(cx, |term, cx| {
                            term.paste_text(&clean, cx);
                        });
                    }
                }
            }
        }
        self.close_terminal_context_menu(cx);
        self.focus_pane(pane_id, window, cx);
    }

    fn choose_attachments(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let Some(files) = rfd::AsyncFileDialog::new().pick_files().await else {
                return;
            };
            let attachments: Vec<_> = files
                .iter()
                .filter_map(|file| attachments::from_path(file.path()))
                .collect();
            let _ = this.update(cx, |this, cx| {
                if let Some(pane) = this.pane_mut(pane_id) {
                    if !pane.chat.active {
                        return;
                    }
                    let mut changed = false;
                    for attachment in attachments {
                        changed |= attachments::push_unique(&mut pane.attachments, attachment);
                    }
                    if changed {
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Stage one file on a pane's input bar and focus it so the user
    /// can see what will be sent.
    fn attach_image(
        &mut self,
        pane_id: usize,
        attachment: attachments::AttachedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pane) = self.pane_mut(pane_id).filter(|pane| !pane.is_browser()) else {
            return false;
        };
        if !attachments::push_unique(&mut pane.attachments, attachment) {
            return false;
        }
        let input_state = pane.input_state.clone();

        self.focused_pane_id = Some(pane_id);
        if let Some(input_state) = input_state {
            input_state.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
        true
    }

    /// Ctrl/Cmd+V with an image on the clipboard attaches it. Returns false
    /// when the clipboard holds no image so the editor's normal text paste
    /// can run.
    fn attach_image_from_clipboard(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        self.attach_images_from_item(pane_id, &item, window, cx)
    }

    /// Attach clipboard pixel data, or fall back to a copied image path.
    /// File managers often place a path (`/tmp/shot.png` or `file:///tmp/shot.png`)
    /// on the clipboard instead of the pixels, which used to be pasted as text.
    fn attach_images_from_item(
        &mut self,
        pane_id: usize,
        item: &gpui::ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(image) = item.entries().iter().find_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(image),
            ClipboardEntry::String(_) | ClipboardEntry::ExternalPaths(_) => None,
        }) && let Some(attachment) = attachments::from_clipboard_image(image)
        {
            return self.attach_image(pane_id, attachment, window, cx);
        }

        let Some(text) = item.text() else {
            return false;
        };
        // Only reinterpret copied paths inside agent panes: in a shell a
        // copied path is usually meant as literal text for a command.
        let agent_running = self.pane_ref(pane_id).is_some_and(|pane| {
            pane.is_agent
                || pane.agent_active
                || status::foreground_process_names(pane.pid)
                    .iter()
                    .any(|name| status::is_agent_process(name))
        });
        if !agent_running {
            return false;
        }

        let mut found = attachments::from_clipboard_text(&text);
        if found.is_empty() {
            return false;
        }

        let mut attached = false;
        while let Some(attachment) = found.pop() {
            attached |= self.attach_image(pane_id, attachment, window, cx);
        }
        attached
    }

    /// Stage files dropped anywhere in the window on the active pane.
    fn drop_external_files(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let pane_id = self.focused_pane_id.or_else(|| {
            self.active_session()
                .and_then(|session| session.panes.first().map(|pane| pane.id))
        });
        let Some(pane_id) = pane_id else {
            return false;
        };

        self.drop_external_files_on_pane(pane_id, paths, window, cx)
    }

    fn drop_external_files_on_pane(
        &mut self,
        pane_id: usize,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut attached = false;
        for path in paths.paths() {
            if let Some(attachment) = attachments::from_path(path) {
                attached |= self.attach_image(pane_id, attachment, window, cx);
            }
        }
        attached
    }

    fn remove_attachment(&mut self, pane_id: usize, index: usize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id)
            && index < pane.attachments.len()
        {
            pane.attachments.remove(index);
            cx.notify();
        }
    }

    fn open_image_viewer(
        &mut self,
        attachment: &attachments::AttachedFile,
        cx: &mut Context<Self>,
    ) {
        self.image_viewer = Some(ImageViewerState {
            path: attachment.path.clone(),
            name: attachment.name.clone(),
            dimensions: attachment.dimensions(),
        });
        cx.notify();
    }

    fn select_all_in_terminal(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id)
            && let Some(pane) = self
                .projects
                .get(p_idx)
                .and_then(|p| p.sessions.get(s_idx))
                .and_then(|s| s.panes.get(pane_idx))
        {
            pane.terminal.update(cx, |term, cx| {
                term.select_all(cx);
            });
        }
        self.close_terminal_context_menu(cx);
        self.focus_pane(pane_id, window, cx);
    }

    fn clear_terminal_buffer(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id)
            && let Some(pane) = self
                .projects
                .get(p_idx)
                .and_then(|p| p.sessions.get(s_idx))
                .and_then(|s| s.panes.get(pane_idx))
        {
            pane.terminal.update(cx, |term, cx| {
                term.send_bytes(b"\x0c", cx);
            });
        }
        self.close_terminal_context_menu(cx);
        self.focus_pane(pane_id, window, cx);
    }

    fn render_terminal_context_menu_layer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(menu) = self.terminal_context_menu else {
            return div().into_any_element();
        };

        let pane_id = menu.pane_id;
        let tokens = self.tokens(cx);

        let has_selection = self
            .pane_index
            .get(&pane_id)
            .and_then(|&(p_idx, s_idx, pane_idx)| {
                self.projects
                    .get(p_idx)
                    .and_then(|p| p.sessions.get(s_idx))
                    .and_then(|s| s.panes.get(pane_idx))
            })
            .map(|pane| pane.terminal.read(cx).has_selection())
            .unwrap_or(false);

        let left = (menu.position.x - px(5.0)).max(px(10.0));
        let top = (menu.position.y - px(5.0)).max(px(35.0));

        let modal_border: Hsla = if tokens.is_dark {
            rgba(0x2d2e3dff).into()
        } else {
            tokens.card_border
        };
        let modal_bg: Hsla = if tokens.is_dark {
            rgba(0x16171eff).into()
        } else {
            rgba(0xffffffff).into()
        };
        let sep_bg: Hsla = if tokens.is_dark {
            rgba(0xffffff14).into()
        } else {
            rgba(0x00000014).into()
        };

        deferred(
            div()
                .id("terminal-context-menu-layer")
                .size_full()
                .absolute()
                .top_0()
                .left_0()
                .child(
                    div()
                        .id("terminal-context-menu-backdrop")
                        .size_full()
                        .absolute()
                        .top_0()
                        .left_0()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.close_terminal_context_menu(cx);
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, _, _, cx| {
                                this.close_terminal_context_menu(cx);
                            }),
                        ),
                )
                .child(
                    div()
                        .id(("terminal-context-menu-modal", pane_id))
                        .absolute()
                        .top(top)
                        .left(left)
                        .w(px(180.0))
                        .p(px(5.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(modal_border)
                        .bg(modal_bg)
                        .shadow_lg()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        // Copy option
                        .child(
                            div()
                                .id(("context-menu-copy", pane_id))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .py(px(5.5))
                                .rounded(px(5.0))
                                .cursor(if has_selection {
                                    CursorStyle::PointingHand
                                } else {
                                    CursorStyle::Arrow
                                })
                                .opacity(if has_selection { 1.0 } else { 0.45 })
                                .when(has_selection, |this| {
                                    this.hover(|s| s.bg(tokens.sidebar_item_hover))
                                })
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            svg()
                                                .path("icons/copy.svg")
                                                .size(px(13.0))
                                                .text_color(tokens.text_primary),
                                        )
                                        .child(
                                            div()
                                                .font_family(MONO_FONT)
                                                .text_size(px(12.0))
                                                .text_color(tokens.text_primary)
                                                .child("Copy"),
                                        ),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(10.5))
                                        .text_color(tokens.text_muted)
                                        .child("Ctrl+C"),
                                )
                                .when(has_selection, |this| {
                                    this.on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.copy_terminal_selection(pane_id, window, cx);
                                        }),
                                    )
                                }),
                        )
                        // Paste option
                        .child(
                            div()
                                .id(("context-menu-paste", pane_id))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .py(px(5.5))
                                .rounded(px(5.0))
                                .cursor(CursorStyle::PointingHand)
                                .hover(|s| s.bg(tokens.sidebar_item_hover))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            svg()
                                                .path("icons/clipboard.svg")
                                                .size(px(13.0))
                                                .text_color(tokens.text_primary),
                                        )
                                        .child(
                                            div()
                                                .font_family(MONO_FONT)
                                                .text_size(px(12.0))
                                                .text_color(tokens.text_primary)
                                                .child("Paste"),
                                        ),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(10.5))
                                        .text_color(tokens.text_muted)
                                        .child("Ctrl+V"),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.paste_from_clipboard_into_pane(pane_id, window, cx);
                                    }),
                                ),
                        )
                        // Separator
                        .child(div().my(px(2.0)).h(px(1.0)).w_full().bg(sep_bg))
                        // Select All
                        .child(
                            div()
                                .id(("context-menu-select-all", pane_id))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .py(px(5.5))
                                .rounded(px(5.0))
                                .cursor(CursorStyle::PointingHand)
                                .hover(|s| s.bg(tokens.sidebar_item_hover))
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(12.0))
                                        .text_color(tokens.text_primary)
                                        .child("Select All"),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(10.5))
                                        .text_color(tokens.text_muted)
                                        .child("Ctrl+A"),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.select_all_in_terminal(pane_id, window, cx);
                                    }),
                                ),
                        )
                        // Clear Screen
                        .child(
                            div()
                                .id(("context-menu-clear", pane_id))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .py(px(5.5))
                                .rounded(px(5.0))
                                .cursor(CursorStyle::PointingHand)
                                .hover(|s| s.bg(tokens.sidebar_item_hover))
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(12.0))
                                        .text_color(tokens.text_primary)
                                        .child("Clear Screen"),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(10.5))
                                        .text_color(tokens.text_muted)
                                        .child("Ctrl+L"),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.clear_terminal_buffer(pane_id, window, cx);
                                    }),
                                ),
                        ),
                ),
        )
        .priority(200)
        .into_any_element()
    }

    /// Ctrl+R palette: type to filter, arrows to pick, Enter to insert.
    fn render_history_search_layer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(search) = self.history_search.as_ref() else {
            return div().into_any_element();
        };
        let tokens = self.tokens(cx);

        let panel_bg: Hsla = if tokens.is_dark {
            rgba(0x16171eff).into()
        } else {
            rgba(0xffffffff).into()
        };
        let panel_border: Hsla = if tokens.is_dark {
            rgba(0x2d2e3dff).into()
        } else {
            tokens.card_border
        };

        let row_selected_bg: Hsla = if tokens.is_dark {
            rgba(0x262838ff).into()
        } else {
            rgba(0x0000000f).into()
        };
        let row_hover_bg: Hsla = if tokens.is_dark {
            rgba(0x1f2130ff).into()
        } else {
            rgba(0x0000000a).into()
        };

        let rows: Vec<gpui::AnyElement> = if search.matches.is_empty() {
            vec![
                div()
                    .px(px(10.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(tokens.text_muted)
                    .child("No matching commands")
                    .into_any_element(),
            ]
        } else {
            search
                .matches
                .iter()
                .take(10)
                .enumerate()
                .map(|(index, command)| {
                    let selected = index == search.selected;
                    div()
                        .id(("history-search-row", index))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(10.0))
                        .px(px(9.0))
                        .py(px(6.0))
                        .rounded(px(5.0))
                        .cursor(CursorStyle::PointingHand)
                        .when(selected, |this| this.bg(row_selected_bg))
                        .hover(|this| this.bg(row_hover_bg))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .min_w_0()
                                .child(
                                    div()
                                        .w(px(16.0))
                                        .text_size(px(10.0))
                                        .text_color(if selected {
                                            rgba(0x38bdf8ff).into()
                                        } else {
                                            tokens.text_muted
                                        })
                                        .child(format!("{}", index + 1)),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(12.5))
                                        .text_color(tokens.text_primary)
                                        .truncate()
                                        .child(command.clone()),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(tokens.text_muted)
                                .child("↵"),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.accept_history_search_index(index, window, cx);
                            }),
                        )
                        .into_any_element()
                })
                .collect()
        };

        deferred(
            div()
                .id("history-search-layer")
                .size_full()
                .absolute()
                .top_0()
                .left_0()
                .child(
                    div()
                        .id("history-search-backdrop")
                        .size_full()
                        .absolute()
                        .top_0()
                        .left_0()
                        .bg(rgba(0x00000055))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_history_search(window, cx);
                            }),
                        ),
                )
                .child(
                    div()
                        .id("history-search-panel")
                        .absolute()
                        .top(px(80.0))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .w(px(560.0))
                                .max_w(relative(0.95))
                                .p(px(10.0))
                                .rounded(px(10.0))
                                .border_1()
                                .border_color(panel_border)
                                .bg(panel_bg)
                                .shadow_lg()
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .on_action(cx.listener(
                                    |this, _: &gpui_component::input::Enter, window, cx| {
                                        this.accept_history_search(window, cx);
                                    },
                                ))
                                .on_key_down(cx.listener(
                                    |this, event: &KeyDownEvent, window, cx| {
                                        match event.keystroke.key.as_str() {
                                            "down" => {
                                                cx.stop_propagation();
                                                this.move_history_search_selection(1, cx);
                                            }
                                            "up" => {
                                                cx.stop_propagation();
                                                this.move_history_search_selection(-1, cx);
                                            }
                                            "escape" => {
                                                cx.stop_propagation();
                                                this.close_history_search(window, cx);
                                            }
                                            _ => {}
                                        }
                                    },
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(10.5))
                                                .font_weight(gpui::FontWeight::BOLD)
                                                .text_color(tokens.text_muted)
                                                .child("SEARCH COMMAND HISTORY"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.0))
                                                .text_color(tokens.text_muted)
                                                .child("Esc to cancel"),
                                        ),
                                )
                                .child(
                                    div().font_family(MONO_FONT).text_size(px(13.0)).child(
                                        Input::new(&search.input)
                                            .appearance(true)
                                            .bordered(true)
                                            .px_2()
                                            .py_1(),
                                    ),
                                )
                                .child(
                                    div()
                                        .id("history-search-results")
                                        .max_h(px(300.0))
                                        .overflow_y_scrollbar()
                                        .flex()
                                        .flex_col()
                                        .gap(px(1.0))
                                        .children(rows),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(tokens.text_muted)
                                        .child("↑↓ navigate · Enter insert · Esc cancel"),
                                ),
                        ),
                ),
        )
        .priority(210)
        .into_any_element()
    }

    #[allow(dead_code)]
    fn toggle_plus_menu(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(project) = self.projects.get_mut(p_idx) else {
            return;
        };
        let Some(session) = project.sessions.get_mut(s_idx) else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        pane.plus_menu_open = !pane.plus_menu_open;
        cx.notify();
    }

    fn run_quick_action(
        &mut self,
        pane_id: usize,
        cmd: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(project) = self.projects.get_mut(p_idx) else {
            return;
        };
        let Some(session) = project.sessions.get_mut(s_idx) else {
            return;
        };
        let Some(pane) = session.panes.get_mut(pane_idx) else {
            return;
        };
        pane.plus_menu_open = false;
        let accepted = pane.terminal.update(cx, |term, cx| {
            term.send_text_with_completion(&format!("{cmd}\r"), cx)
                .is_ok()
        });
        if !accepted {
            return;
        }
        if pane.command_history.last().map(|s| s.as_str()) != Some(cmd) {
            pane.command_history.push(cmd.to_string());
        }
        if let Some(input_state) = &pane.input_state {
            input_state.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        }
        cx.notify();
    }
}
