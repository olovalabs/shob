impl AppState {
    fn render_chat_message(
        message: &ChatMessage,
        options: ChatMessageOptions<'_>,
        host: &UiHost,
        render: ChatRenderContext<'_>,
    ) -> AnyElement {
        let ChatMessageOptions {
            pane_id,
            conversation,
            index,
            running,
            copy_range,
        } = options;
        let ChatRenderContext { tokens, window, cx } = render;
        let mut paint_marker = None;
        if !message.text.is_empty()
            && let Some((turn, started)) = message.turn
            && crate::stream_timing::turn_started_at(turn) == Some(started)
        {
            let kind = match message.role {
                ChatRole::Assistant => Some(crate::stream_timing::DeltaKind::Answer),
                ChatRole::Thinking if message.expanded => {
                    Some(crate::stream_timing::DeltaKind::Reasoning)
                }
                _ => None,
            };
            if let Some(kind) = kind
                && crate::stream_timing::turn_paint_due(turn, kind)
            {
                // The canvas paints with this row, so the sample is the frame
                // that actually made the text visible rather than the next
                // scheduled frame.
                paint_marker = Some(
                    canvas(
                        |_, _, _| (),
                        move |_, _, _, _| {
                            if crate::stream_timing::turn_started_at(turn) == Some(started) {
                                crate::stream_timing::turn_painted(turn, kind);
                            }
                        },
                    )
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .size(px(1.0))
                    .into_any_element(),
                );
            }
        }
        let inner: AnyElement = match message.role {
            ChatRole::User => {
                let text = message
                    .literal
                    .get_or_init(|| SharedString::from(message.text.clone()))
                    .clone();
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_end()
                    .child(
                        div()
                            .max_w(relative(0.70))
                            .min_w_0()
                            .px(px(16.0))
                            .py(px(10.0))
                            .rounded(px(18.0))
                            .bg(tokens.chat().user_background)
                            .border_1()
                            .border_color(tokens.chat().user_border)
                            .text_size(px(14.0))
                            .line_height(agent_appearance::LINE_HEIGHT)
                            .child(
                                TextView::markdown(
                                    SharedString::from(format!(
                                        "agent-user-{pane_id}-{conversation}-{index}"
                                    )),
                                    text.clone(),
                                    window,
                                    cx,
                                )
                                .style(tokens.chat().markdown(cx.theme().is_dark(), true, cx.theme().highlight_theme.clone()))
                                .selectable(true)
                                .text_size(px(14.0))
                                .text_color(tokens.chat().user_foreground),
                            ),
                    )
                    .child(
                        div()
                            .id(("user-message-actions", index))
                            .opacity(0.)
                            .group_hover("chat-message", |style| style.opacity(1.))
                            .in_focus(|style| style.opacity(1.))
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap(px(2.0))
                            .child(chat_copy_button(
                                format!("chat-copy-user-{pane_id}-{index}").into(),
                                text.clone(),
                                "Copy prompt".into(),
                                tokens,
                            ))
                            .child({
                                let text = text.clone();
                                div()
                                    .id(SharedString::from(format!(
                                        "chat-edit-user-{pane_id}-{index}"
                                    )))
                                    .tab_index(0)
                                    .size(px(22.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(5.0))
                                    .text_color(tokens.text_muted)
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(move |style| {
                                        style
                                            .bg(tokens.sidebar_item_hover)
                                            .text_color(tokens.text_primary)
                                    })
                                    .tooltip(|window, cx| {
                                        gpui_component::tooltip::Tooltip::new(
                                            "Edit in empty composer",
                                        )
                                        .build(window, cx)
                                    })
                                    .on_click(host.listener(move |this, _, window, cx| {
                                        if let Some(input) = this
                                            .pane_ref(pane_id)
                                            .and_then(|pane| pane.input_state.clone())
                                            && input.read(cx).value().trim().is_empty()
                                        {
                                            input.update(cx, |input, cx| {
                                                input.set_value(text.clone(), window, cx);
                                                input.focus(window, cx);
                                            });
                                        }
                                    }))
                                    .child(
                                        svg()
                                            .path("icons/pencil.svg")
                                            .size(px(14.0))
                                            .text_color(tokens.text_muted),
                                    )
                            }),
                    )
                    .into_any_element()
            }
            ChatRole::Assistant => {
                let id =
                    SharedString::from(format!("agent-markdown-{pane_id}-{conversation}-{index}"));
                let markdown_text_color = tokens.chat().foreground;
                let mut body = div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .text_size(px(14.0))
                    .line_height(agent_appearance::LINE_HEIGHT)
                    .text_color(markdown_text_color);

                if let Some(view) = message.stream.as_ref().filter(|_| message.streaming) {
                    if !view.settled.is_empty() {
                        body = body.child(chat_markdown(
                            id.clone(),
                            view.settled.clone(),
                            px(14.0),
                            rems(1.0),
                            markdown_text_color,
                            ChatRenderContext { tokens, window, cx },
                        ));
                    }
                    if !view.tail.is_empty() {
                        body = body.child(chat_stream_tail(
                            SharedString::from(format!(
                                "agent-markdown-{pane_id}-{conversation}-{index}-live"
                            )),
                            view,
                            px(14.0),
                            rems(1.0),
                            markdown_text_color,
                            ChatRenderContext { tokens, window, cx },
                        ));
                    }
                    if view.plain_tail {
                        body = body.child(chat_stream_copy(host, pane_id, index, tokens));
                    }
                } else {
                    body = body.child(chat_markdown(
                        id,
                        message.markdown(),
                        px(14.0),
                        rems(1.0),
                        markdown_text_color,
                        ChatRenderContext { tokens, window, cx },
                    ));
                }

                if let Some((start, end)) = copy_range {
                    body = body.child(
                        div()
                            .id(SharedString::from(format!(
                                "chat-copy-assistant-{pane_id}-{index}"
                            )))
                            .size(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .cursor(CursorStyle::PointingHand)
                            .text_color(tokens.text_muted)
                            .hover(move |style| {
                                style
                                    .bg(tokens.sidebar_item_hover)
                                    .text_color(tokens.text_primary)
                            })
                            .tooltip(|window, cx| {
                                gpui_component::tooltip::Tooltip::new("Copy full response")
                                    .build(window, cx)
                            })
                            .on_click(host.listener(move |this, _, _, cx| {
                                if let Some(pane) = this.pane_ref(pane_id) {
                                    let text = pane
                                        .chat
                                        .visible_messages()
                                        .get(start..=end)
                                        .unwrap_or_default()
                                        .iter()
                                        .filter(|message| {
                                            message.role == ChatRole::Assistant
                                                && !message.text.is_empty()
                                        })
                                        .map(|message| message.text.as_str())
                                        .collect::<Vec<_>>()
                                        .join("\n\n");
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                                }
                            }))
                            .child(
                                svg()
                                    .path("icons/copy.svg")
                                    .size(px(12.0))
                                    .text_color(tokens.text_muted),
                            ),
                    );
                }
                body.into_any_element()
            }
            ChatRole::Thinking => {
                let on_toggle = host.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.toggle_chat_tool_expanded(pane_id, index, cx);
                });
                let mut row = chat_thinking_row(
                    pane_id,
                    index,
                    message,
                    running,
                    ChatRenderContext { tokens, window, cx },
                    on_toggle,
                );
                if message.expanded && message.stream.as_ref().is_some_and(|view| view.plain_tail) {
                    row = row.child(chat_stream_copy(host, pane_id, index, tokens));
                }
                row.into_any_element()
            }
            ChatRole::Status => div()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    div()
                        .max_w(px(520.0))
                        .truncate()
                        .px(px(10.0))
                        .py(px(3.0))
                        .rounded_full()
                        .bg(tokens.card_hover)
                        .text_size(px(11.0))
                        .text_color(tokens.text_muted)
                        .child(message.text.clone()),
                )
                .into_any_element(),
            ChatRole::Tool => {
                if let Some(tool) = &message.tool {
                    let on_toggle = host.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.toggle_chat_tool_expanded(pane_id, index, cx);
                    });
                    let on_show_more = host.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.toggle_chat_tool_show_full(pane_id, index, cx);
                    });
                    chat_tool_card(pane_id, index, tool, tokens, host, on_toggle, on_show_more)
                        .into_any_element()
                } else {
                    div()
                        .w_full()
                        .min_w_0()
                        .h(px(22.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(4.0))
                        .text_size(px(12.0))
                        .text_color(tokens.text_muted)
                        .child(message.text.clone())
                        .into_any_element()
                }
            }
            ChatRole::Error => div()
                .w_full()
                .min_w_0()
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .bg(tokens.block_failure.opacity(0.08))
                .border_1()
                .border_color(tokens.block_failure.opacity(0.35))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(tokens.block_failure)
                        .child("Error"),
                )
                .child(
                    div()
                        .w_full()
                        .whitespace_normal()
                        .font_family(MONO_FONT)
                        .text_size(px(11.5))
                        .line_height(agent_appearance::LINE_HEIGHT)
                        .text_color(tokens.block_failure)
                        .child(message.text.clone()),
                )
                .into_any_element(),
        };

        if let Some(marker) = paint_marker {
            div()
                .group("chat-message")
                .debug_selector(move || format!("chat-message-{index}"))
                .w_full()
                .min_w_0()
                .relative()
                .child(marker)
                .child(inner)
                .into_any_element()
        } else {
            div().group("chat-message").debug_selector(move || format!("chat-message-{index}")).w_full().min_w_0().child(inner).into_any_element()
        }
    }

    fn worker_avatar(avatar: usize, pixel: f32) -> gpui::Div {
        let (pattern, color) = subagent_chat::AVATARS[avatar];
        div()
            .relative()
            .size(px(pixel * 5.0))
            .flex_shrink_0()
            .children(pattern.into_iter().enumerate().flat_map(|(y, row)| {
                (0..5)
                    .filter(move |x| row & (0b10000 >> x) != 0)
                    .map(move |x| {
                        div()
                            .absolute()
                            .left(px(x as f32 * pixel))
                            .top(px(y as f32 * pixel))
                            .size(px(pixel))
                            .bg(rgba(color))
                    })
            }))
    }

    fn render_worker_row(
        pane_id: usize,
        row: &WorkerRowSnapshot,
        tokens: ThemeTokens,
        host: &UiHost,
    ) -> AnyElement {
        let index = row.index;
        let id = row.id.clone();
        div()
            .id(SharedString::from(format!(
                "subagent-{pane_id}-{index}-{id}"
            )))
            .debug_selector({
                let id = id.clone();
                move || format!("subagent-row-{id}")
            })
            .w_full()
            .min_w_0()
            .h(px(30.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(4.0))
            .text_size(px(14.0))
            .cursor(CursorStyle::PointingHand)
            .hover(move |style| style.bg(tokens.sidebar_item_hover))
            .child(Self::worker_avatar(row.avatar, 3.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(tokens.text_primary)
                    .child(row.title.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(tokens.text_muted)
                    .child(row.status),
            )
            .on_click(host.listener(move |this, _, window, cx| {
                this.open_worker_view(pane_id, index, &id, window, cx);
            }))
            .into_any_element()
    }

    fn open_worker_view(
        &mut self,
        pane_id: usize,
        index: usize,
        id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let rows = subagent_chat::rows(pane.chat.visible_messages());
        let Some(row) = rows
            .iter()
            .find(|row| row.index == index && row.worker.id == id)
        else {
            return;
        };
        let shob_session_root = crate::paths::shob_sessions_dir();
        let legacy_session_root = crate::paths::data_dir().join("provider/agent/sessions");
        let path = row.worker.transcript.clone().filter(|path| {
            path.starts_with(&shob_session_root) || path.starts_with(&legacy_session_root)
        });
        let tool_id = pane.chat.visible_messages()[index]
            .tool
            .as_ref()
            .and_then(|tool| tool.id.clone());
        let id = id.to_owned();
        let worker_id = id.clone();
        let load_id = id.clone();
        let load = cx.spawn(async move |this, cx| {
            let path = match path {
                Some(path) => Some(path),
                None => {
                    cx.background_executor()
                        .spawn(async move {
                            subagent_chat::find_legacy_transcript(&worker_id, &tool_id?)
                        })
                        .await
                }
            };
            let Some(path) = path else { return };
            let mut previous = None;
            loop {
                let file = path.clone();
                let snapshot = cx
                    .background_executor()
                    .spawn(async move {
                        let meta = std::fs::metadata(&file).ok()?;
                        let stamp = (meta.len(), meta.modified().ok());
                        if previous == Some(stamp) {
                            return None;
                        }
                        Some((stamp, subagent_chat::read_transcript(&file)?))
                    })
                    .await;
                if let Some((stamp, messages)) = snapshot {
                    previous = Some(stamp);
                    if this
                        .update(cx, |this, cx| {
                            if let Some(view) = this
                                .pane_mut(pane_id)
                                .and_then(|pane| pane.chat.worker_view.as_mut())
                                .filter(|view| view.id == load_id && view.index == index)
                            {
                                view.messages = messages;
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
            }
        });
        pane.chat.worker_view = Some(WorkerView {
            index,
            id,
            scroll: gpui::ScrollHandle::new(),
            messages: Vec::new(),
            expanded: HashSet::new(),
            full_output: HashSet::new(),
            _load: Some(load),
        });
        cx.notify();
    }

    fn render_worker_view(
        &self,
        pane: &TerminalPane,
        view: &WorkerView,
        row: &subagent_chat::Row<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane_id = pane.id;
        if view.scroll.max_offset().y + view.scroll.offset().y <= px(24.0) {
            view.scroll.scroll_to_bottom();
        }
        let tokens = self.tokens(cx);
        let worker = row.worker;
        let back = div()
            .id(("subagent-back", pane_id))
            .debug_selector(|| "subagent-back".to_string())
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .cursor(CursorStyle::PointingHand)
            .hover(move |style| style.bg(tokens.sidebar_item_hover))
            .text_size(px(12.0))
            .text_color(tokens.text_secondary)
            .child(
                svg()
                    .path("icons/arrow-left.svg")
                    .size(px(13.0))
                    .text_color(tokens.text_secondary),
            )
            .child("Back to parent chat")
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(pane) = this.pane_mut(pane_id) {
                    pane.chat.worker_view = None;
                    this.focus_pane_input(pane_id, window, cx);
                }
            }));

        let mut body = div()
            .id(("subagent-transcript", pane_id))
            .debug_selector(|| "subagent-transcript".to_string())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&view.scroll)
            .px(px(24.0))
            .py(px(16.0))
            .flex()
            .flex_col()
            .gap(px(14.0));
        let conversation = format!("worker-{}-{}", view.index, view.id);
        let messages = view.messages.iter();
        let host = UiHost::new(cx.weak_entity());
        let mut has_reply = false;
        for (index, message) in messages.enumerate() {
            has_reply |= matches!(
                message.role,
                ChatRole::Assistant | ChatRole::Thinking | ChatRole::Tool
            );
            let mut adjusted = None;
            if view.expanded.contains(&index) || view.full_output.contains(&index) {
                let mut row = (**message).clone();
                if let Some(tool) = &mut row.tool {
                    tool.expanded ^= view.expanded.contains(&index);
                    tool.show_full ^= view.full_output.contains(&index);
                } else {
                    row.expanded ^= view.expanded.contains(&index);
                }
                adjusted = Some(row);
            }
            let message = adjusted.as_ref().unwrap_or(message);
            body = body.child(Self::render_chat_message(
                message,
                ChatMessageOptions {
                    pane_id,
                    conversation: &conversation,
                    index,
                    running: message.streaming,
                    copy_range: None,
                },
                &host,
                ChatRenderContext { tokens, window, cx },
            ));
        }
        if !has_reply {
            if let Some(result) = row.result.filter(|result| !result.is_empty()) {
                body = body.child(chat_markdown(
                    format!("subagent-report-{pane_id}-{conversation}").into(),
                    result.to_owned(),
                    px(13.0),
                    rems(0.6),
                    tokens.text_primary,
                    ChatRenderContext { tokens, window, cx },
                ));
            } else if view.messages.is_empty() {
                let text = match worker.status_label() {
                    "Starting" | "Running" => "Waiting for the subagent’s first message…",
                    _ => "No transcript or report was saved for this subagent.",
                };
                body = body.child(
                    div()
                        .text_size(px(13.0))
                        .text_color(tokens.text_muted)
                        .child(text),
                );
            }
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(16.0))
                    .py(px(10.0))
                    .border_b_1()
                    .border_color(tokens.border_subtle)
                    .child(back)
                    .child(
                        div()
                            .mt(px(9.0))
                            .px(px(8.0))
                            .pb(px(4.0))
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(Self::worker_avatar(row.avatar, 4.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .child(
                                        div()
                                            .truncate()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(tokens.text_primary)
                                            .child(worker.display_title()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(tokens.text_muted)
                                            .child(format!("Subagent · {}", worker.status_label())),
                                    ),
                            ),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    fn render_agent_chat(
        &self,
        pane: &TerminalPane,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        pane.chat
            .view_visible
            .set(self.active_view == ActiveView::Workspace);
        let mut view = pane.chat.view.borrow_mut();
        let view = view.get_or_insert_with(|| {
            let app = cx.weak_entity();
            let pane_id = pane.id;
            cx.new(|_| AgentChatView {
                app,
                pane_id,
                #[cfg(test)]
                renders: 0,
            })
        });
        view.clone()
            .cached(
                if self.settings.agent_layout
                    && pane.chat.visible_messages().is_empty()
                    && pane.chat.worker_view.is_none()
                {
                    StyleRefinement::default()
                        .w_full()
                        .h(px(105.0))
                        .flex_shrink_0()
                } else {
                    StyleRefinement::default().size_full()
                },
            )
            .into_any_element()
    }
}
