impl AppState {
    fn render_agent_chat_body(
        &self,
        pane: &TerminalPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let visible = pane.chat.visible_messages();
        if let Some(view) = &pane.chat.worker_view {
            let workers = subagent_chat::rows(visible);
            if let Some(worker) = workers
                .iter()
                .find(|row| row.index == view.index && row.worker.id == view.id)
            {
                return self.render_worker_view(pane, view, worker, window, cx);
            }
        }
        let render_start = std::time::Instant::now();
        let tokens = self.tokens(cx);
        let conversation = pane.chat.conversation_key.as_str();
        let pane_id = pane.id;
        let is_autoscroll = pane.chat.autoscroll;
        if is_autoscroll && !pane.chat.list_state.is_following_tail() {
            pane.chat.list_state.set_follow_mode(FollowMode::Tail);
        }

        let running = pane.chat.visible_running();
        let row_start = std::time::Instant::now();
        let row_cache = Self::build_chat_rows(&pane.chat, running);
        let cached = row_cache.borrow();
        let rows = &cached.rows;
        let active_row = cached.state.active_row;
        crate::stream_timing::record_since("ui.render_rows", row_start);

        let row_count = rows.len();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(conversation, &mut hasher);
        pane.chat
            .sync_rows(row_count, std::hash::Hasher::finish(&hasher));
        // Markdown parsing and virtual block measurements can finish without
        // another transcript event. Refresh only previously measured viewport
        // rows on this child notification, preserving the list's scroll anchor.
        let first_visible = pane.chat.list_state.logical_scroll_top().item_ix;
        let mut visible_end = first_visible;
        while visible_end < row_count {
            let Some(bounds) = pane.chat.list_state.bounds_for_item(visible_end) else {
                break;
            };
            if bounds.top() >= window.viewport_size().height {
                break;
            }
            visible_end += 1;
        }
        if visible_end > first_visible {
            pane.chat
                .list_state
                .remeasure_items(first_visible..visible_end);
        }
        for index in pane.chat.dirty_messages.take() {
            if let Some(row) = rows.iter().position(|row| chat_row_contains(row, index)) {
                pane.chat.remeasure_row(row);
            }
        }
        if pane.chat.active_dirty.replace(false) {
            let latest_content = active_row.or_else(|| {
                rows.iter()
                    .rposition(|row| !matches!(row, ChatRow::Running { .. }))
            });
            if pane.chat.activity_dirty.replace(false) {
                for (index, row) in rows.iter().enumerate().rev() {
                    if matches!(row, ChatRow::Message { message, .. } if message.role == ChatRole::User)
                    {
                        break;
                    }
                    // A section can fold or receive more output after the reply starts.
                    if (matches!(row, ChatRow::ToolGroup { .. } | ChatRow::Subagents { .. })
                        || matches!(row, ChatRow::Message { message, .. } if message.tool.as_ref().is_some_and(|tool| tool.expanded)))
                        && Some(index) != latest_content
                    {
                        pane.chat.remeasure_row(index);
                    }
                }
            }
            if let Some(row) = latest_content {
                pane.chat.remeasure_row(row);
            }
        }
        drop(cached);
        if !pane.chat.scroll_handler_installed.get() {
            let weak = cx.weak_entity();
            let id = pane_id;
            pane.chat
                .list_state
                .set_scroll_handler(move |event, _, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(pane) = this.pane_mut(id) {
                            // Bottom-aligned lists report their end with no scroll
                            // anchor; tail-follow itself is re-armed during layout,
                            // after this callback has run.
                            let following = event.is_following_tail || !event.is_scrolled;
                            if pane.chat.autoscroll != following {
                                pane.chat.autoscroll = following;
                                pane.chat.notify_content(cx);
                            }
                        }
                    });
                });
            pane.chat.scroll_handler_installed.set(true);
        }
        let default_screen: Option<AnyElement> = if pane.chat.visible_messages().is_empty() {
            let screen = div()
                .id(("agent-default-screen", pane_id))
                .debug_selector(|| "agent-default-screen".to_string())
                .when(self.settings.agent_layout, |screen| screen.w_full())
                .when(!self.settings.agent_layout, |screen| screen.size_full())
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(16.0))
                .child(
                    svg()
                        .path("shob-agent-wordmark.svg")
                        .w_full()
                        .max_w(px(300.0))
                        .h(px(105.0))
                        .text_color(tokens.text_primary),
                );
            Some(screen.into_any_element())
        } else {
            None
        };

        if let Some(screen) = default_screen {
            return div()
                .relative()
                .when(self.settings.agent_layout, |view| view.w_full())
                .when(!self.settings.agent_layout, |view| view.size_full())
                .child(screen)
                .into_any_element();
        }

        let host = UiHost::new(cx.weak_entity());
        let snapshot = Rc::new(ChatSnapshot {
            focused_layout: self.settings.agent_layout,
            pane_id,
            conversation: conversation.into(),
            tokens,
            rows: row_cache,
            host: host.clone(),
        });
        let row_snapshot = Rc::clone(&snapshot);
        let list_state = pane.chat.list_state.clone();
        let chat_view = div()
            .id(("agent-chat", pane_id))
            .debug_selector(|| "agent-chat".to_string())
            .size_full()
            .flex()
            .justify_center()
            .child(
                list(list_state.clone(), move |index, window, cx| {
                    Self::render_chat_row(&row_snapshot, index, window, cx)
                })
                .size_full()
                .px(px(24.0))
                .pt(px(8.0))
                .pb(px(20.0)),
            );

        let scroll_to_bottom_btn = if !is_autoscroll {
            Some(
                div()
                    .id(("scroll-to-bottom-btn", pane_id))
                    .debug_selector(|| "scroll-to-bottom-btn".to_string())
                    .absolute()
                    .bottom(if self.settings.agent_layout {
                        px(8.0)
                    } else {
                        px(16.0)
                    })
                    .when(self.settings.agent_layout, |button| {
                        button
                            .left(relative(0.5))
                            .ml(-px(16.0))
                            .size(px(32.0))
                            .justify_center()
                    })
                    .when(!self.settings.agent_layout, |button| {
                        button.right(px(24.0)).gap(px(5.0)).px(px(10.0)).py(px(5.0))
                    })
                    .cursor(CursorStyle::PointingHand)
                    .flex()
                    .items_center()
                    .rounded_full()
                    .bg(tokens.pane_header_bg)
                    .border_1()
                    .border_color(tokens.border)
                    .shadow_md()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(tokens.text_primary)
                    .hover(|s| s.bg(tokens.sidebar_item_hover))
                    .child(
                        svg()
                            .path(if self.settings.agent_layout {
                                "icons/arrow-down.svg"
                            } else {
                                "icons/chevron-down.svg"
                            })
                            .size(if self.settings.agent_layout {
                                px(16.0)
                            } else {
                                px(11.0)
                            })
                            .text_color(tokens.text_primary),
                    )
                    .when(!self.settings.agent_layout, |button| {
                        button.child("Scroll to bottom")
                    })
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("Scroll to bottom").build(window, cx)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        host.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(pane) = this.pane_mut(pane_id) {
                                pane.chat.autoscroll = true;
                                pane.chat.scroll_to_end();
                                cx.notify();
                            }
                        }),
                    ),
            )
        } else {
            None
        };

        let scrollbar = div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .child(
                gpui_component::scroll::Scrollbar::vertical(&list_state)
                    .id(("agent-chat-scrollbar", pane_id)),
            );

        crate::stream_timing::record_since("ui.render_chat", render_start);
        div()
            .relative()
            .size_full()
            .child(chat_view)
            .when(!self.settings.agent_layout, |view| view.child(scrollbar))
            .children(scroll_to_bottom_btn)
            .into_any_element()
    }

    fn build_chat_rows(
        chat: &PaneChat,
        running: bool,
    ) -> std::rc::Rc<std::cell::RefCell<ChatRowCache>> {
        let visible = chat.visible_messages();
        let mut cache = chat.row_cache.borrow_mut();
        if cache.conversation.as_deref() != Some(chat.conversation_key.as_str())
            || visible.len() < cache.message_count
            || cache.state.cursor > visible.len()
        {
            *cache = ChatRowCache {
                conversation: Some(chat.conversation_key.clone()),
                dirty: true,
                workers_dirty: true,
                ..Default::default()
            };
        }
        let tail = visible
            .last()
            .map_or(0, |message| Arc::as_ptr(message) as usize);
        if !cache.dirty && (visible.len() != cache.message_count || tail != cache.tail) {
            let from = visible.len().min(cache.message_count).saturating_sub(1);
            cache.invalidate_from(from);
            cache.activity = None;
            if visible[from..].iter().any(|message| {
                message
                    .tool
                    .as_ref()
                    .is_some_and(|tool| !tool.workers.is_empty())
            }) {
                cache.invalidate_workers();
            }
        }
        if running != cache.running {
            let from = cache
                .state
                .last_assistant_row
                .and_then(|row| cache.checkpoints.get(row))
                .map_or(visible.len().saturating_sub(1), |checkpoint| {
                    checkpoint.cursor
                });
            cache.invalidate_from(from);
            cache.activity = None;
        }
        if !cache.dirty {
            return std::rc::Rc::clone(&chat.row_cache);
        }
        if cache.workers_dirty {
            cache.workers = subagent_chat::rows(visible)
                .into_iter()
                .map(|row| WorkerRowSnapshot {
                    index: row.index,
                    id: row.worker.id.clone(),
                    avatar: row.avatar,
                    title: row.worker.display_title(),
                    status: worker_status_label(row.worker.status_label()),
                })
                .collect();
            cache.workers_dirty = false;
        }
        cache.message_count = visible.len();
        cache.running = running;
        cache.tail = tail;
        cache.dirty = false;
        let ChatRowCache {
            rows,
            checkpoints,
            state,
            workers,
            activity,
            ..
        } = &mut *cache;
        #[cfg(test)]
        let kept_rows = rows.len();
        let ChatRowCheckpoint {
            mut cursor,
            mut turn_start,
            mut last_assistant_row,
            mut active_row,
        } = *state;
        let mut worker_index = workers.partition_point(|worker| worker.index < cursor);
        while cursor < visible.len() {
            let checkpoint = ChatRowCheckpoint {
                cursor,
                turn_start,
                last_assistant_row,
                active_row,
            };
            let message = &visible[cursor];
            if message
                .tool
                .as_ref()
                .is_some_and(|tool| !tool.workers.is_empty())
            {
                let start = cursor;
                while cursor < visible.len()
                    && visible[cursor]
                        .tool
                        .as_ref()
                        .is_some_and(|tool| !tool.workers.is_empty())
                {
                    cursor += 1;
                }
                if worker_index < workers.len() && workers[worker_index].index < cursor {
                    let mut block = Vec::new();
                    while worker_index < workers.len() && workers[worker_index].index < cursor {
                        block.push(workers[worker_index].clone());
                        worker_index += 1;
                    }
                    if block
                        .iter()
                        .any(|row| matches!(row.status, "is starting" | "is working"))
                    {
                        active_row = Some(rows.len());
                    }
                    checkpoints.push(checkpoint);
                    rows.push(ChatRow::Subagents { start, rows: block });
                }
                continue;
            }
            if message.role == ChatRole::Tool
                && message
                    .tool
                    .as_ref()
                    .is_some_and(|tool| !chat_batch::is_batch(&tool.name))
            {
                let start = cursor;
                let mut has_running_call = false;
                while let Some(message) = visible.get(cursor) {
                    let Some(tool) = message.tool.as_ref().filter(|tool| {
                        message.role == ChatRole::Tool
                            && tool.workers.is_empty()
                            && !chat_batch::is_batch(&tool.name)
                    }) else {
                        break;
                    };
                    has_running_call |= tool.state == ToolState::Running;
                    cursor += 1;
                }
                let live = has_running_call || (running && cursor == visible.len());
                let open = live
                    || chat
                        .expanded_tool_groups
                        .get(chat.conversation_key.as_str())
                        .is_some_and(|groups| groups.contains(&start));
                if running && cursor == visible.len() {
                    active_row = Some(rows.len());
                }
                checkpoints.push(checkpoint);
                rows.push(ChatRow::ToolGroup {
                    start,
                    messages: visible[start..cursor].to_vec(),
                    live,
                    open,
                });
                continue;
            }
            if message.role == ChatRole::User {
                turn_start = cursor + 1;
                last_assistant_row = None;
            }
            let copy_range = if message.role == ChatRole::Assistant && !message.text.is_empty() {
                if let Some(previous) = last_assistant_row
                    && let ChatRow::Message { copy_range, .. } = &mut rows[previous]
                {
                    *copy_range = None;
                }
                last_assistant_row = Some(rows.len());
                Some((turn_start, cursor))
            } else {
                None
            };
            let live = running && cursor + 1 == visible.len();
            if message.streaming
                || message
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.state == ToolState::Running)
            {
                active_row = Some(rows.len());
            }
            checkpoints.push(checkpoint);
            rows.push(ChatRow::Message {
                index: cursor,
                message: Arc::clone(message),
                live,
                copy_range,
            });
            cursor += 1;
        }
        *state = ChatRowCheckpoint {
            cursor,
            turn_start,
            last_assistant_row,
            active_row,
        };
        if running {
            if let Some(previous) = last_assistant_row
                && let ChatRow::Message { copy_range, .. } = &mut rows[previous]
            {
                *copy_range = None;
            }
            let (label, tooltip) = activity.get_or_insert_with(|| {
                (
                    chat_activity(visible).into(),
                    chat_activity_tooltip(visible).into(),
                )
            });
            checkpoints.push(*state);
            rows.push(ChatRow::Running {
                label: label.clone(),
                tooltip: tooltip.clone(),
            });
        }
        #[cfg(test)]
        {
            let built = rows.len() - kept_rows;
            cache.built_rows += built;
        }
        std::rc::Rc::clone(&chat.row_cache)
    }

    fn render_chat_row(
        snapshot: &ChatSnapshot,
        index: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let cached = snapshot.rows.borrow();
        let Some(row) = cached.rows.get(index) else {
            return div().into_any_element();
        };
        let tokens = snapshot.tokens;
        let inner: AnyElement = match row {
            ChatRow::Subagents { start, rows } => {
                let rendered = rows
                    .iter()
                    .map(|row| {
                        Self::render_worker_row(snapshot.pane_id, row, tokens, &snapshot.host)
                    })
                    .collect::<Vec<_>>();
                div()
                    .debug_selector({
                        let start = *start;
                        move || format!("subagent-list-{start}")
                    })
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .pb(px(10.0))
                    .border_b_1()
                    .border_color(tokens.border_subtle)
                    .child(
                        div()
                            .pb(px(7.0))
                            .text_size(px(14.0))
                            .text_color(tokens.text_muted)
                            .child("Subagents"),
                    )
                    .children(rendered)
                    .into_any_element()
            }
            ChatRow::ToolGroup {
                start,
                messages,
                live,
                open,
            } => {
                let start = *start;
                let mut group = div().w_full().min_w_0().flex().flex_col();
                if !live {
                    let mut titles = Vec::new();
                    let mut failed = 0;
                    for tool in messages.iter().filter_map(|message| message.tool.as_ref()) {
                        if let Some(id) = tool.id.as_deref() {
                            crate::stream_timing::tool_card_painted(id);
                        }
                        failed += usize::from(tool.state == ToolState::Failed);
                        let title = tool.display.view.title.as_str();
                        if !titles.contains(&title) {
                            titles.push(title);
                        }
                    }
                    let count = messages.len();
                    let mut label = format!("Used {count} {}", titles.join(", "));
                    if failed > 0 {
                        label.push_str(&format!(" ({failed} failed)"));
                    }
                    let pane_id = snapshot.pane_id;
                    group = group.child(
                        div()
                            .id(SharedString::from(format!("tool-group-{pane_id}-{start}")))
                            .debug_selector(move || format!("tool-group-{start}"))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .text_size(px(14.0))
                            .text_color(tokens.text_secondary)
                            .hover(move |style| style.text_color(tokens.text_primary))
                            .cursor(CursorStyle::PointingHand)
                            .on_mouse_down(
                                MouseButton::Left,
                                snapshot
                                    .host
                                    .listener(move |this, _: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        this.toggle_chat_tool_group(pane_id, start, cx);
                                    }),
                            )
                            .child(div().min_w_0().truncate().child(label))
                            .child(div().flex_1())
                            .child(
                                svg()
                                    .path(if *open {
                                        "icons/chevron-down.svg"
                                    } else {
                                        "icons/chevron-right.svg"
                                    })
                                    .size(px(12.0))
                                    .flex_shrink_0()
                                    .text_color(tokens.text_muted),
                            ),
                    );
                }
                if *open {
                    group = group.children(messages.iter().enumerate().map(|(offset, message)| {
                        Self::render_chat_message(
                            message,
                            ChatMessageOptions {
                                pane_id: snapshot.pane_id,
                                conversation: snapshot.conversation.as_ref(),
                                index: start + offset,
                                running: *live,
                                copy_range: None,
                            },
                            &snapshot.host,
                            ChatRenderContext { tokens, window, cx },
                        )
                    }));
                }
                group.into_any_element()
            }
            ChatRow::Message {
                index,
                message,
                live,
                copy_range,
            } => Self::render_chat_message(
                message,
                ChatMessageOptions {
                    pane_id: snapshot.pane_id,
                    conversation: snapshot.conversation.as_ref(),
                    index: *index,
                    running: *live,
                    copy_range: *copy_range,
                },
                &snapshot.host,
                ChatRenderContext { tokens, window, cx },
            ),
            ChatRow::Running { label, tooltip } => div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .py(px(2.0))
                .id(("agent-activity", snapshot.pane_id))
                .tooltip({
                    let tooltip = tooltip.clone();
                    move |_, cx| {
                        cx.new(|_| gpui_component::tooltip::Tooltip::new(tooltip.clone()))
                            .into()
                    }
                })
                // A fixed frame, not a repeating animation: a running turn can last
                // hours, and a 650 ms repeat would repaint the window the whole time.
                .child(
                    div()
                        .size(px(16.0))
                        .flex_shrink_0()
                        .child(sidebar_loader(0.0, tokens.text_muted)),
                )
                .child(
                    div()
                        .text_color(tokens.text_muted)
                        .text_size(px(12.0))
                        .min_w_0()
                        .truncate()
                        .child(label.clone()),
                )
                .into_any_element(),
        };
        let live_tools = matches!(row, ChatRow::ToolGroup { live: true, .. });
        let content = div()
            .debug_selector(|| "agent-chat-row".to_string())
            .w_full()
            .min_w_0()
            .max_w(px(784.0))
            .px(px(16.0))
            .when(
                snapshot.focused_layout && matches!(row, ChatRow::Message { message, .. } if message.role == ChatRole::User),
                |row| row.pt(px(24.0)).pb(px(24.0)),
            )
            .when(
                !snapshot.focused_layout || !matches!(row, ChatRow::Message { message, .. } if message.role == ChatRole::User),
                |row| row.pb(px(if live_tools { 0.0 } else { 12.0 })),
            )
            .child(inner);
        div()
            .w_full()
            .flex()
            .justify_center()
            .child(content)
            .into_any_element()
    }
    fn open_project_script_form(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Script name"));
        let command =
            cx.new(|cx| InputState::new(window, cx).placeholder("Command, e.g. cargo test"));
        name.update(cx, |state, cx| state.focus(window, cx));
        self.project_script_form = Some((pane_id, name, command));
        cx.notify();
    }

    fn save_project_script(&mut self, cx: &mut Context<Self>) {
        let Some((pane, name, command)) = &self.project_script_form else {
            return;
        };
        let name = name.read(cx).value().trim().to_owned();
        let command = command.read(cx).value().trim().to_owned();
        if name.is_empty() || name.len() > 128 || command.is_empty() || command.len() > 4096 {
            return;
        }
        let Some(&(project, _, _)) = self.pane_index.get(pane) else {
            return;
        };
        let scripts = self
            .settings
            .project_scripts
            .entry(self.projects[project].path.clone())
            .or_default();
        if scripts.len() >= 32 {
            return;
        }
        scripts.push(settings::ProjectScript { name, command });
        self.project_script_form = None;
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn run_project_script(
        &mut self,
        source: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(&(project, _, _)) = self.pane_index.get(&source) else {
            return;
        };
        let Some(script) = self
            .settings
            .project_scripts
            .get(&self.projects[project].path)
            .and_then(|s| s.get(index))
            .cloned()
        else {
            return;
        };
        let next_id = self.next_id;
        self.create_project_session(project, false, window, cx);
        if self.focused_pane_id != Some(next_id) {
            return;
        }
        let Some(input) = self.pane_ref(next_id).and_then(|p| p.input_state.clone()) else {
            return;
        };
        set_input_text(&input, script.command, window, cx);
        self.execute_pane_input(next_id, window, cx);
    }

    fn open_project_editor(
        &mut self,
        pane_id: usize,
        selected: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(directory) = self
            .pane_ref(pane_id)
            .map(|pane| self.agent_working_directory(pane))
        else {
            return;
        };
        let explicit_selection = selected.is_some();
        let preferred = selected.or_else(|| self.settings.preferred_editor.clone());
        self.editor_picker_open = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let (editors, result) = cx
                .background_executor()
                .spawn(async move {
                    let editors = Self::discover_project_editors();
                    let editor = Self::preferred_project_editor(&editors, preferred.as_deref());
                    let result = if let Some((name, executable)) = editor {
                        std::process::Command::new(executable)
                            .arg(&directory)
                            .spawn()
                            .map(|_| Some(name.clone()))
                    } else {
                        paths::open_in_file_manager(&directory).map(|_| None)
                    };
                    (editors, result)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.available_editors = editors;
                match result {
                    Ok(Some(name))
                        if explicit_selection || this.settings.preferred_editor.is_none() =>
                    {
                        this.settings.preferred_editor = Some(name);
                        this.schedule_workspace_save(cx);
                    }
                    Err(error) => window.push_notification(
                        Notification::new()
                            .title("Could not open project")
                            .message(error.to_string())
                            .with_type(NotificationType::Error),
                        cx,
                    ),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_project_editors(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let editors = cx
                .background_executor()
                .spawn(async { Self::discover_project_editors() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.available_editors = editors;
                cx.notify();
            });
        })
        .detach();
    }

    fn preferred_project_editor<'a>(
        editors: &'a [(String, PathBuf)],
        preferred: Option<&str>,
    ) -> Option<&'a (String, PathBuf)> {
        editors
            .iter()
            .find(|(name, _)| preferred == Some(name.as_str()))
            .or_else(|| editors.first())
    }
}
