impl AppState {
    fn render_shob_input_bar(
        &self,
        pane: &TerminalPane,
        show_welcome_context: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let pane_id = pane.id;
        let focused_composer = pane.chat.active;
        let tokens = self.tokens(cx);
        let agent_cwd = pane.chat.active.then(|| self.agent_working_directory(pane));
        let cwd_display = agent_cwd
            .as_ref()
            .map(|path| Self::format_path_display(path))
            .unwrap_or_else(|| self.pane_cwd_display(pane));
        let git_branch = if pane.chat.active {
            None
        } else {
            self.pane_git_branch(pane)
        };
        let model_pill = pane
            .chat
            .selected_model
            .as_ref()
            .map(|model| model.id.clone())
            .unwrap_or_else(|| "No model selected".to_string());
        let model_provider_icon = pane
            .chat
            .selected_model
            .as_ref()
            .map(|model| provider_icon_path(&model.provider));

        let gradient_ascii = self.palette_scheme == PaletteScheme::MidnightGarden && tokens.is_dark;
        let bar_border: Hsla = if gradient_ascii {
            tokens.editor_bar_border
        } else if tokens.is_dark {
            rgba(0x1d1d1dff).into()
        } else {
            tokens.pane_border
        };
        let bar_bg: Hsla = if gradient_ascii {
            tokens.editor_bar_bg
        } else if tokens.is_dark {
            rgba(0x050505ff).into()
        } else {
            tokens.pane_header_bg
        };
        let focused_bar_bg = tokens.chat().composer;
        let focused_bar_border = tokens.editor_bar_border;
        let badge_border: Hsla = if gradient_ascii {
            tokens.card_border
        } else if tokens.is_dark {
            rgba(0x282828ff).into()
        } else {
            rgba(0x00000018).into()
        };
        let badge_bg: Hsla = if gradient_ascii {
            tokens.card_hover
        } else if tokens.is_dark {
            rgba(0x111111ff).into()
        } else {
            rgba(0x0000000a).into()
        };
        let dir_icon_color: Hsla = if gradient_ascii {
            tokens.text_secondary
        } else if tokens.is_dark {
            rgba(0xb2b2c3ff).into()
        } else {
            tokens.text_secondary
        };
        let dir_text_color: Hsla = if gradient_ascii {
            tokens.text_primary
        } else if tokens.is_dark {
            rgba(0xe5e6fcff).into()
        } else {
            tokens.text_primary
        };
        let git_icon_color: Hsla = if tokens.is_dark {
            rgba(0xadc993ff).into()
        } else {
            rgba(0x2d7a2dff).into()
        };
        let git_text_color: Hsla = if tokens.is_dark {
            rgba(0xd9fcb8ff).into()
        } else {
            rgba(0x236823ff).into()
        };
        let input_element = if let Some(input_state) = &pane.input_state {
            let browser_mentioned = pane.chat.active
                && agent_bridge::has_browser_mention(input_state.read(cx).value().as_str());
            div()
                .id(("shob-input-wrapper", pane_id))
                .debug_selector(|| "agent-composer-input".to_string())
                .w_full()
                .min_h(px(30.0))
                .max_h(px(220.0))
                .overflow_y_scrollbar()
                .flex()
                .items_start()
                .font_family(if pane.chat.active {
                    cx.theme().font_family.clone()
                } else {
                    MONO_FONT.into()
                })
                .text_size(px(14.0))
                .key_context(SHOB_INPUT_CONTEXT)
                .on_action(cx.listener(move |this, _: &CompleteCommand, window, cx| {
                    cx.stop_propagation();
                    if !this.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
                        this.start_completion(pane_id, window, cx);
                    }
                }))
                .on_action(cx.listener(
                    move |this, _: &gpui_component::input::Escape, window, cx| {
                        if this
                            .pane_ref(pane_id)
                            .is_some_and(|p| p.chat.auth.is_some())
                        {
                            cx.stop_propagation();
                            this.close_agent_auth(pane_id, window, cx);
                        } else if let Some(pane) = this.pane_mut(pane_id)
                            && (pane.chat.info_open == Some(true)
                                || pane.chat.info_open_compact
                                || pane.workspace_menu_open
                                || pane.project_menu_open
                                || pane.branch_picker.is_some())
                        {
                            cx.stop_propagation();
                            pane.chat.info_open = Some(false);
                            pane.chat.info_open_compact = false;
                            pane.workspace_menu_open = false;
                            pane.project_menu_open = false;
                            pane.branch_picker = None;
                            cx.notify();
                        }
                    },
                ))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    this.handle_pane_input_key(pane_id, event, window, cx);
                }))
                .child(
                    div().flex_1().flex().items_center().child(
                        Input::new(input_state)
                            .appearance(false)
                            .bordered(false)
                            .focus_bordered(false)
                            .cleanable(false)
                            .when(focused_composer, |input| input.text_size(agent_appearance::FONT_SIZE).font_family(cx.theme().font_family.clone()))
                            .px_0()
                            .py_0()
                            .when(browser_mentioned, |input| {
                                input.prefix(
                                    div()
                                        .debug_selector(|| "agent-browser-icon".to_string())
                                        .self_start()
                                        .h(rems(1.25))
                                        .flex()
                                        .items_center()
                                        .child(img("icons/browser-color.png").size(px(16.0))),
                                )
                            }),
                    ),
                )
                .into_any_element()
        } else {
            div()
                .id(("shob-input-wrapper-empty", pane_id))
                .w_full()
                .min_h(px(30.0))
                .into_any_element()
        };

        let completion_menu = self.render_completion_menu(pane, &tokens, cx);

        let plus_menu = if pane.plus_menu_open {
            Some(
                div()
                    .id(("shob-plus-menu", pane_id))
                    .absolute()
                    .bottom(px(100.0))
                    .left(px(12.0))
                    .w(px(220.0))
                    .p(px(6.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(if pane.chat.active || gradient_ascii {
                        tokens.card_bg
                    } else if tokens.is_dark {
                        rgba(0x1e1f22ff).into()
                    } else {
                        rgba(0xffffffff).into()
                    })
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(4.0))
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(tokens.text_muted)
                            .child("QUICK ACTIONS"),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(3.0))
                            .border_b_1()
                            .border_color(tokens.card_border)
                            .text_size(px(11.0))
                            .text_color(tokens.text_secondary)
                            .child(format!("📁 {}", cwd_display)),
                    )
                    .child(Self::render_quick_action_item(
                        "Clear Terminal",
                        "clear",
                        "icons/x.svg",
                        pane_id,
                        &tokens,
                        cx,
                    ))
                    .child(Self::render_quick_action_item(
                        "List Files",
                        "ls -la",
                        "icons/folder.svg",
                        pane_id,
                        &tokens,
                        cx,
                    ))
                    .child(Self::render_quick_action_item(
                        "Git Status",
                        "git status",
                        "icons/status-check.svg",
                        pane_id,
                        &tokens,
                        cx,
                    ))
                    .child(Self::render_quick_action_item(
                        "Working Directory",
                        "pwd",
                        "icons/monitor.svg",
                        pane_id,
                        &tokens,
                        cx,
                    )),
            )
        } else {
            None
        };

        let model_selector = pane.chat.active.then(|| {
            let bounds = pane.model_picker_bounds.clone();
                        div()
                            .id(("select-agent-model", pane_id))
                            .debug_selector(|| "select-agent-model-pill".to_string())
                            .relative()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .h(px(28.0))
                            .px(px(8.0))
                            .rounded(px(7.0))
                            .when(!self.settings.agent_layout, |pill| pill.bg(tokens.card_bg))
                            .text_size(px(13.0))
                            .text_color(tokens.text_primary)
                            .cursor_pointer()
                            .hover(|view| view.bg(tokens.sidebar_item_hover))
                            .when_some(model_provider_icon, |pill, icon| {
                                pill.child(svg().path(icon).size(px(15.0)).flex_shrink_0().text_color(tokens.text_primary))
                            })
                            .child(div().min_w_0().max_w(px(224.0)).truncate().child(model_pill))
                            .when(focused_composer, |pill| pill.child(
                                svg().path("icons/chevron-down.svg").size(px(12.0))
                                    .flex_shrink_0().text_color(tokens.text_muted)
                            ))
                            .child(canvas(move |area, _, _| bounds.set(area), |_, _, _, _| {})
                                .absolute().top_0().left_0().size_full())
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_agent_auth(pane_id, AgentDialogPage::Models, window, cx);
                            }))
        });
        let attachment_strip = self.render_attachment_strip(pane, &tokens, cx);
        let drop_hover_border: Hsla = tokens.ring;
        let drop_hover_bg: Hsla = if pane.chat.active {
            tokens.ring.opacity(0.08)
        } else if tokens.is_dark {
            rgba(0x11251cff).into()
        } else {
            rgba(0x2d7a2d12).into()
        };
        div()
            .id(("shob-composer-stack", pane_id))
            .w_full()
            .when(pane.chat.active, |stack| stack.font_family(cx.theme().font_family.clone()).text_color(tokens.chat().foreground).max_w(agent_appearance::COLUMN_WIDTH).mx_auto().px(agent_appearance::GUTTER).pb(px(8.0)))
            .flex()
            .flex_col()
            .children(pane.chat.active.then(|| self.render_agent_follow_ups(pane, &tokens, cx)).flatten())
            .children(pane.chat.active.then(|| self.render_agent_activity(pane, &tokens, cx)).flatten())
            .children(pane.chat.active.then(|| self.render_agent_recovery(pane, &tokens, cx)).flatten())
            .children(
                self.render_agent_task_list(pane, &tokens, cx)
                    .map(|list| div().pb(px(8.0)).child(list)),
            )
            .children(pane.chat.active.then(|| self.render_agent_request(pane, &tokens, window, cx)).flatten())
            .child(
        div()
            .id(("shob-input-bar", pane_id))
            .debug_selector(|| "shob-input-bar".to_string())
            .relative()
            .w_full()
            .flex()
            .flex_col()
            .px(if focused_composer { px(16.0) } else { px(12.0) })
            .pt(px(11.0))
            .pb(px(10.0))
            .when(pane.chat.active, |bar| {
                bar.rounded(px(20.0)).border_1().bg(focused_bar_bg)
            })
            .when(!pane.chat.active, |bar| bar.border_t_1().bg(bar_bg))
            .border_color(if pane.chat.active { focused_bar_border } else { bar_border })
            .can_drop(|value, _, _| {
                value.downcast_ref::<ExternalPaths>().is_some_and(|paths| {
                    paths
                        .paths()
                        .iter()
                        .any(|path| path.is_file())
                })
            })
            .on_drop::<ExternalPaths>(cx.listener(
                move |this, paths: &ExternalPaths, window, cx| {
                    this.drop_external_files_on_pane(pane_id, paths, window, cx);
                },
            ))
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style.bg(drop_hover_bg).border_color(drop_hover_border)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if this.pane_ref(pane_id).is_some_and(|p| {
                        p.chat.auth.is_some() || p.chat.active && !p.chat.requests.is_empty()
                    }) {
                        return;
                    }
                    this.focus_pane_input(pane_id, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_terminal_context_menu(pane_id, event.position, window, cx);
                }),
            )
            .children(plus_menu)
            .child(
                div().relative().w_full().h(px(0.0)).children(completion_menu.map(|menu| {
                    div().absolute().bottom(px(6.0)).left_0().w_full().child(menu)
                })),
            )
            // Keep the same controls in both layouts, with the prompt first in agent layout.
            .child(
                div().flex().flex_col()
                    .when(pane.chat.active, |row| row.flex_col_reverse())
                    .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .when(pane.chat.active, |row| row.child(
                        div().id(("agent-attach-file", pane_id))
                            .debug_selector(|| "agent-attach-file".into())
                            .size(px(26.0)).flex_shrink_0()
                            .flex().items_center().justify_center()
                            .rounded_full().cursor_pointer()
                            .hover(|style| style.bg(tokens.pane_header_btn_hover))
                            .tooltip(|window, cx| gpui_component::tooltip::Tooltip::new("Add attachments").build(window, cx))
                            .child(svg().path("icons/paperclip.svg").size(if focused_composer { px(18.0) } else { px(16.0) }).text_color(tokens.text_muted))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.choose_attachments(pane_id, cx);
                            }))
                    ))
                    .when(pane.chat.active, |row| {
                        row.child(self.render_agent_permission(pane, false, window, cx))
                    })
                    .when(pane.chat.active, |row| row.child(div().flex_1().min_w_0()))
                    .children(model_selector)
                    .when(pane.chat.active && pane.chat.visible_running(), |row| row.child(
                        div().id(("agent-stop", pane_id))
                            .size(px(28.0)).flex_shrink_0().rounded_full()
                            .flex().items_center().justify_center()
                            .bg(tokens.text_primary)
                            .hover(|button| button.bg(tokens.text_secondary))
                            .cursor(CursorStyle::PointingHand)
                            .tooltip(|window, cx| {
                                gpui_component::tooltip::Tooltip::new("Stop").build(window, cx)
                            })
                            .child(div().size(px(9.0)).flex_shrink_0().rounded(px(1.0)).bg(focused_bar_bg))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.handle_pane_ctrl_c(pane_id, window, cx);
                            })),
                    ))
                    .when(pane.chat.active && !pane.chat.visible_running(), |row| row.child(
                        div().id(("agent-send", pane_id)).debug_selector(|| "agent-send".into())
                            .size(px(28.0)).flex_shrink_0().rounded_full()
                            .flex().items_center().justify_center()
                            .bg(tokens.text_primary).hover(|button| button.bg(tokens.text_secondary))
                            .cursor_pointer()
                            .tooltip(|window, cx| gpui_component::tooltip::Tooltip::new("Send message").build(window, cx))
                            .child(svg().path("icons/arrow-up.svg").size(px(18.0)).text_color(focused_bar_bg))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.submit_agent_prompt(pane_id, window, cx);
                            })),
                    ))
                    // Agent metadata lives in the right-side info drawer.
                    .when(!pane.chat.active, |row| row.child(
                        div()
                            .id(("shob-badge-cwd", pane_id))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .h(px(22.0))
                            .px(px(7.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(badge_border)
                            .bg(badge_bg)
                            .child(
                                svg()
                                    .path("icons/folder.svg")
                                    .size(px(12.0))
                                    .text_color(dir_icon_color),
                            )
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_size(px(12.0))
                                    .text_color(dir_text_color)
                                    .child(cwd_display),
                            ),
                    ))
                    // Badge 2: Git branch
                    .when_some(git_branch, |this, branch| {
                        this.child(
                            div()
                                .id(("shob-badge-git", pane_id))
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .h(px(22.0))
                                .px(px(7.0))
                                .rounded(px(5.0))
                                .border_1()
                                .border_color(badge_border)
                                .bg(badge_bg)
                                .child(
                                    svg()
                                        .path("icons/git-branch.svg")
                                        .size(px(12.0))
                                        .text_color(git_icon_color),
                                )
                                .child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_size(px(12.0))
                                        .text_color(git_text_color)
                                        .child(branch),
                                ),
                        )
                    })
            )
            // Row 2: Staged image attachments
            .children(attachment_strip)
            // Row 3: Input Row
            .child(
                div()
                    .w_full()
                    .when(pane.chat.active, |input| input.mb(px(16.0)))
                    .when(!pane.chat.active, |input| input.mt(px(12.0)))
                    .child(input_element),
            ))
            .when(!pane.chat.active, |bar| {
                bar.child(
                    div()
                        .w_full()
                        .mt(px(7.0))
                        .h(px(12.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .text_size(px(10.0))
                        .text_color(tokens.text_muted)
                        .child(
                            "Tab complete · ↑↓ history · Ctrl+R search · Shift+Enter newline · Drop or paste images",
                        ),
                )
            }))
            .when(show_welcome_context, |stack| {
                stack.child(self.render_composer_context_strip(pane, &tokens, false, window, cx))
            })
    }

    /// Shob-style completion popup, anchored above the input bar.
    fn render_completion_menu(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let pane_id = pane.id;
        let menu = pane.completion_menu.as_ref()?;
        if menu.items.is_empty() {
            return None;
        }

        let bg: Hsla = if pane.chat.active {
            tokens.card_bg
        } else if tokens.is_dark {
            rgba(0x16171eff).into()
        } else {
            rgba(0xffffffff).into()
        };
        let border: Hsla = if pane.chat.active {
            tokens.card_border
        } else if tokens.is_dark {
            rgba(0x2d2e3dff).into()
        } else {
            tokens.card_border
        };
        let row_selected_bg: Hsla = if pane.chat.active {
            tokens.sidebar_item_active
        } else if tokens.is_dark {
            rgba(0x262838ff).into()
        } else {
            rgba(0x0000000f).into()
        };
        let row_hover_bg: Hsla = if pane.chat.active {
            tokens.sidebar_item_hover
        } else if tokens.is_dark {
            rgba(0x1f2130ff).into()
        } else {
            rgba(0x0000000a).into()
        };

        let rows: Vec<gpui::AnyElement> = menu
            .items
            .iter()
            .enumerate()
            .take(9)
            .map(|(index, item)| {
                let selected = index == menu.selected;
                let kind_color: Hsla = match item.kind {
                    completion::CompletionKind::Directory => rgba(0x54aeffff).into(),
                    completion::CompletionKind::File => tokens.text_secondary,
                    completion::CompletionKind::Branch => rgba(0x9ecb7bff).into(),
                    completion::CompletionKind::History => rgba(0xd7ba7dff).into(),
                    completion::CompletionKind::Command => rgba(0xc5a3ffff).into(),
                };
                div()
                    .id(("completion-item", pane_id * 1000 + index))
                    .flex()
                    .items_center()
                    .when(!pane.chat.active, |this| this.justify_between())
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded(if pane.chat.active { px(14.0) } else { px(4.0) })
                    .cursor(CursorStyle::PointingHand)
                    .when(selected, |this| this.bg(row_selected_bg))
                    .hover(|this| this.bg(row_hover_bg))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .min_w_0()
                            .when(pane.chat.active, |this| {
                                let icon = match item.display.as_str() {
                                    "/help" => "icons/info.svg",
                                    "/diff" => "icons/git-branch.svg",
                                    "/new" => "icons/plus.svg",
                                    "/copy" => "icons/copy.svg",
                                    "/version" => "icons/terminal.svg",
                                    "/auth" => "icons/settings.svg",
                                    "/model" => "icons/command.svg",
                                    "/fast" => "icons/sparkle.svg",
                                    _ => "icons/settings.svg",
                                };
                                if item.value == "@browser" {
                                    this.child(img("icons/browser-color.png").size(px(16.0)))
                                } else {
                                    this.child(
                                        svg()
                                            .path(icon)
                                            .size(px(14.0))
                                            .text_color(tokens.text_secondary),
                                    )
                                }
                            })
                            .when(!pane.chat.active, |this| {
                                this.child(
                                    div()
                                        .w(px(14.0))
                                        .text_size(px(10.0))
                                        .text_color(kind_color)
                                        .child(if item.is_dir { "▸" } else { "·" }),
                                )
                            })
                            .child(
                                div()
                                    .when(!pane.chat.active, |this| this.font_family(MONO_FONT))
                                    .text_size(px(12.5))
                                    .text_color(tokens.text_primary)
                                    .truncate()
                                    .child(item.display.clone()),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_size(if pane.chat.active { px(12.0) } else { px(10.0) })
                            .text_color(tokens.text_muted)
                            .truncate()
                            .child(item.detail.clone()),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.accept_completion_at(pane_id, index, window, cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .id(("completion-menu", pane_id))
                .debug_selector(|| "completion-menu".to_string())
                .occlude()
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    this.close_completion_menu(pane_id, cx);
                }))
                .w_full()
                .when(!pane.chat.active, |menu| menu.max_w(px(440.0)))
                .max_h(if pane.chat.active {
                    px(320.0)
                } else {
                    px(240.0)
                })
                .overflow_y_scrollbar()
                .p(px(5.0))
                .rounded(if pane.chat.active { px(20.0) } else { px(8.0) })
                .border_1()
                .border_color(border)
                .bg(bg)
                .shadow_lg()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .children(rows)
                .into_any_element(),
        )
    }

    /// The pane whose provider/model dialog is open, if any.
    fn open_agent_auth_pane(&self) -> Option<usize> {
        self.projects
            .iter()
            .flat_map(|project| project.sessions.iter())
            .flat_map(|session| session.panes.iter())
            .find(|pane| pane.chat.auth.is_some())
            .map(|pane| pane.id)
    }
}
