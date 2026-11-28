impl AppState {
    /// Keep ghost text and the completion menu in sync with typed text.
    fn on_pane_input_changed(
        &mut self,
        pane_id: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
            if let Some(input) = self
                .pane_ref(pane_id)
                .and_then(|pane| pane.input_state.clone())
            {
                let value = input.read(cx).text().to_string();
                let items = agent_slash_completions(&value);
                let browser_mentioned = agent_bridge::has_browser_mention(&value);
                let prompt_empty = value.trim().is_empty();
                let mut update_root = false;
                if let Some(pane) = self.pane_mut(pane_id) {
                    update_root = pane.input_browser_mentioned != browser_mentioned
                        || (!pane.chat.follow_ups.is_empty() && pane.input_prompt_empty != prompt_empty)
                        || pane.completion_menu.is_some()
                        || !items.is_empty();
                    pane.input_browser_mentioned = browser_mentioned;
                    pane.input_prompt_empty = prompt_empty;
                    let selected = pane
                        .completion_menu
                        .as_ref()
                        .filter(|menu| menu.token == value)
                        .map_or(0, |menu| menu.selected);
                    pane.completion_menu = if items.is_empty() {
                        None
                    } else {
                        Some(CompletionMenuState {
                            start: 0,
                            end: value.len(),
                            token: value,
                            selected: selected.min(items.len() - 1),
                            items,
                        })
                    };
                }
                if input.read(cx).inline_completion_text().is_some() {
                    input.update(cx, |state, cx| state.clear_inline_completion(cx));
                }
                if update_root {
                    cx.notify();
                }
            }
            return;
        }
        let Some((value, cursor, cwd, entries, input)) = self.input_context(pane_id, cx) else {
            return;
        };
        let menu_open = self
            .pane_ref(pane_id)
            .is_some_and(|pane| pane.completion_menu.is_some());

        if menu_open {
            // Typing filters an open menu instead of dismissing it.
            self.refresh_completion_menu(pane_id, &value, cursor, &cwd, &entries);
            input.update(cx, |state, cx| state.clear_inline_completion(cx));
            return;
        }

        let suggestion = completion::history_suggestion(&value, cursor, &entries);
        input.update(cx, |state, cx| match suggestion {
            Some(text) if !text.is_empty() => state.set_inline_completion_text(text, cx),
            _ => state.clear_inline_completion(cx),
        });
    }

    /// Recompute the open completion menu for the current input value.
    fn refresh_completion_menu(
        &mut self,
        pane_id: usize,
        value: &str,
        cursor_utf16: usize,
        cwd: &Path,
        entries: &[String],
    ) {
        let cursor = completion::utf16_to_byte(value, cursor_utf16);
        let result = completion::complete(value, cursor, cwd, entries);
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        match result {
            Some(result) => {
                let token = value
                    .get(result.start..result.end)
                    .unwrap_or_default()
                    .to_string();
                let previous = pane
                    .completion_menu
                    .as_ref()
                    .filter(|menu| menu.token == token)
                    .map(|menu| menu.selected)
                    .unwrap_or(0);
                pane.completion_menu = Some(CompletionMenuState {
                    start: result.start,
                    end: result.end,
                    token,
                    selected: previous.min(result.items.len().saturating_sub(1)),
                    items: result.items,
                });
            }
            None => pane.completion_menu = None,
        }
    }

    fn close_completion_menu(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.completion_menu = None;
        }
        cx.notify();
    }

    fn move_completion_selection(&mut self, pane_id: usize, delta: isize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id)
            && let Some(menu) = pane.completion_menu.as_mut()
            && !menu.items.is_empty()
        {
            let len = menu.items.len() as isize;
            menu.selected = (menu.selected as isize + delta).rem_euclid(len) as usize;
        }
        cx.notify();
    }

    /// Tab: accept the only candidate, or open the menu when there are several.
    fn start_completion(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((value, cursor, cwd, entries, input)) = self.input_context(pane_id, cx) else {
            return;
        };

        // A visible ghost suggestion is the best candidate; Tab accepts it.
        let ghost = input.read(cx).inline_completion_text().map(str::to_string);
        if ghost.is_some() {
            input.update(cx, |state, cx| {
                state.accept_inline_completion(window, cx);
            });
            cx.notify();
            return;
        }

        // Even before the deferred change handler has painted ghost text, Tab
        // completes from history.
        if let Some(suffix) = completion::history_suggestion(&value, cursor, &entries) {
            let cursor = cursor + completion::utf16_len(&suffix);
            set_input_text_with_cursor(&input, format!("{value}{suffix}"), cursor, window, cx);
            cx.notify();
            return;
        }

        let cursor_byte = completion::utf16_to_byte(&value, cursor);
        let Some(result) = completion::complete(&value, cursor_byte, &cwd, &entries) else {
            self.close_completion_menu(pane_id, cx);
            return;
        };

        if result.items.len() == 1 {
            let item = result.items[0].clone();
            self.apply_completion(pane_id, result.start, result.end, item, window, cx);
        } else {
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.completion_menu = Some(CompletionMenuState {
                    start: result.start,
                    end: result.end,
                    token: value
                        .get(result.start..result.end)
                        .unwrap_or_default()
                        .to_string(),
                    items: result.items,
                    selected: 0,
                });
            }
            cx.notify();
        }
    }

    fn accept_completion_at(
        &mut self,
        pane_id: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.pane_ref(pane_id).and_then(|pane| {
            let menu = pane.completion_menu.as_ref()?;
            let item = menu.items.get(index)?.clone();
            Some((menu.start, menu.end, item))
        });
        let Some((start, end, item)) = selected else {
            return;
        };
        if self.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
            let Some(input) = self
                .pane_ref(pane_id)
                .and_then(|pane| pane.input_state.clone())
            else {
                return;
            };
            self.close_completion_menu(pane_id, cx);
            if item.value == "/thinking" || item.value == "@browser" {
                set_input_text(&input, format!("{} ", item.value), window, cx);
            } else {
                set_input_text(&input, item.value, window, cx);
                self.submit_agent_prompt(pane_id, window, cx);
            }
            return;
        }
        self.apply_completion(pane_id, start, end, item, window, cx);
    }

    /// Replace the completing token with `item` and keep the editor usable.
    fn apply_completion(
        &mut self,
        pane_id: usize,
        start: usize,
        end: usize,
        item: completion::CompletionItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((value, _, _, _entries, input)) = self.input_context(pane_id, cx) else {
            return;
        };
        let start = start.min(value.len());
        let end = end.min(value.len());
        if !value.is_char_boundary(start) || !value.is_char_boundary(end) || start > end {
            return;
        }

        let mut new_value = String::with_capacity(value.len() + item.value.len() + 1);
        new_value.push_str(&value[..start]);
        new_value.push_str(&item.value);
        let mut cursor = start + item.value.len();
        new_value.push_str(&value[end..]);

        // Command and branch completions read better with the cursor moved to
        // the next token; file completions stay tight to the path.
        if !item.is_dir && item.kind != completion::CompletionKind::File {
            new_value.insert(cursor, ' ');
            cursor += 1;
        }

        let cursor_utf16 = completion::byte_to_utf16(&new_value, cursor);
        input.update(cx, |state, cx| {
            state.set_value(new_value, window, cx);
            let position = state.text().offset_to_position(
                cursor_utf16.min(completion::utf16_len(state.value().as_str())),
            );
            state.set_cursor_position(position, window, cx);
            state.clear_inline_completion(cx);
        });

        if item.is_dir {
            // Descend into the directory: keep completing the new token.
            if let Some((value, cursor, cwd, entries, _)) = self.input_context(pane_id, cx) {
                if let Some(pane) = self.pane_mut(pane_id) {
                    pane.completion_menu = None;
                }
                self.refresh_completion_menu(pane_id, &value, cursor, &cwd, &entries);
                if let Some(pane) = self.pane_mut(pane_id)
                    && let Some(menu) = pane.completion_menu.as_mut()
                {
                    menu.selected = 0;
                }
            }
        } else if let Some(pane) = self.pane_mut(pane_id) {
            pane.completion_menu = None;
        }

        if let Some(pane) = self.pane_ref(pane_id)
            && let Some(input_state) = pane.input_state.clone()
        {
            input_state.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // History search (Ctrl+R)
    // ---------------------------------------------------------------------

    fn open_history_search(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.history_search.is_some() {
            return;
        }
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let entries = self.pane_history_entries(pane);
        let initial = pane
            .input_state
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search command history")
                .default_value(initial.clone())
        });
        input.update(cx, |state, cx| {
            let position = state
                .text()
                .offset_to_position(completion::utf16_len(&initial));
            state.set_cursor_position(position, window, cx);
            state.focus(window, cx);
        });

        let matches: Vec<String> = shell_history::search(&entries, &initial)
            .into_iter()
            .cloned()
            .collect();

        let subscription = cx.subscribe_in(
            &input,
            window,
            move |_this: &mut Self, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let weak = cx.entity().downgrade();
                    window.defer(cx, move |window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.refresh_history_search(window, cx);
                        });
                    });
                }
            },
        );

        self.history_search = Some(HistorySearchState {
            pane_id,
            input,
            matches,
            selected: 0,
            _subscription: subscription,
        });
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.completion_menu = None;
        }
        cx.notify();
    }

    fn refresh_history_search(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let query = search.input.read(cx).value().to_string();
        let pane_id = search.pane_id;
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let entries = self.pane_history_entries(pane);
        let matches: Vec<String> = shell_history::search(&entries, &query)
            .into_iter()
            .cloned()
            .collect();
        if let Some(search) = self.history_search.as_mut() {
            search.matches = matches;
            search.selected = 0;
        }
        cx.notify();
    }

    fn move_history_search_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(search) = self.history_search.as_mut()
            && !search.matches.is_empty()
        {
            let len = search.matches.len() as isize;
            search.selected = (search.selected as isize + delta).rem_euclid(len) as usize;
        }
        cx.notify();
    }

    fn close_history_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane_id = self.history_search.as_ref().map(|search| search.pane_id);
        self.history_search = None;
        if let Some(pane_id) = pane_id
            && let Some(input) = self
                .pane_ref(pane_id)
                .and_then(|pane| pane.input_state.clone())
        {
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn accept_history_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let Some(command) = search.matches.get(search.selected).cloned() else {
            let pane_id = search.pane_id;
            self.history_search = None;
            if let Some(input) = self
                .pane_ref(pane_id)
                .and_then(|pane| pane.input_state.clone())
            {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
            cx.notify();
            return;
        };
        let pane_id = search.pane_id;
        let input = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.input_state.clone());
        self.history_search = None;
        if let Some(input) = input {
            set_input_text(&input, command, window, cx);
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.history_index = None;
        }
        cx.notify();
    }

    fn accept_history_search_index(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(search) = self.history_search.as_mut() {
            search.selected = index;
        }
        self.accept_history_search(window, cx);
    }

    #[allow(dead_code)]
    fn clear_pane_input(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
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
        if let Some(input_state) = &pane.input_state {
            input_state.update(cx, |state, cx| {
                state.set_value("", window, cx);
                state.focus(window, cx);
            });
        }
        pane.history_index = None;
        pane.pending_draft.clear();
        pane.completion_menu = None;
        cx.notify();
    }

    #[allow(dead_code)]
    fn cancel_pane_job(&mut self, pane_id: usize, cx: &mut Context<Self>) {
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
        pane.terminal.update(cx, |term, cx| {
            term.send_bytes(b"\x03", cx);
        });
        cx.notify();
    }

    fn step_pane_history(
        &mut self,
        pane_id: usize,
        is_up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let Some(input_state) = pane.input_state.clone() else {
            return;
        };
        let entries = shell_history::merge(&pane.shell_history, &pane.command_history);
        if entries.is_empty() {
            return;
        }

        if is_up {
            let new_idx = match pane.history_index {
                None => {
                    pane.pending_draft = input_state.read(cx).value().to_string();
                    entries.len() - 1
                }
                Some(idx) => idx.saturating_sub(1),
            };
            pane.history_index = Some(new_idx);
            if let Some(cmd) = entries.get(new_idx).cloned() {
                set_input_text(&input_state, cmd, window, cx);
            }
        } else if let Some(idx) = pane.history_index {
            if idx + 1 < entries.len() {
                let new_idx = idx + 1;
                pane.history_index = Some(new_idx);
                if let Some(cmd) = entries.get(new_idx).cloned() {
                    set_input_text(&input_state, cmd, window, cx);
                }
            } else {
                pane.history_index = None;
                let draft = std::mem::take(&mut pane.pending_draft);
                set_input_text(&input_state, draft, window, cx);
            }
        }
        cx.notify();
    }
    fn toggle_chat_tool_expanded(&mut self, pane_id: usize, index: usize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id) {
            if let Some(view) = &mut pane.chat.worker_view {
                if !view.expanded.remove(&index) {
                    view.expanded.insert(index);
                }
                if let Some(msg) = view.messages.get_mut(index)
                    && (msg.tool.is_some() || msg.role == ChatRole::Thinking)
                {
                    let msg = Arc::make_mut(msg);
                    let expanded = msg.tool.as_mut().map_or(&mut msg.expanded, |tool| &mut tool.expanded);
                    *expanded = !*expanded;
                    if *expanded {
                        view.list_state.pause_following_tail();
                    }
                }
                view.invalidate_message_row(index);
                pane.chat.notify_worker_view(cx);
                cx.notify();
                return;
            }
            let messages = pane.chat.visible_messages_mut();
            if let Some(msg) = messages.get_mut(index)
                && (msg.tool.is_some() || msg.role == ChatRole::Thinking)
            {
                let msg = Arc::make_mut(msg);
                let expanded = msg.tool.as_mut().map_or(&mut msg.expanded, |tool| &mut tool.expanded);
                *expanded = !*expanded;
                if *expanded {
                    pane.chat.list_state.pause_following_tail();
                }
                pane.chat.invalidate_message_row(index);
                cx.notify();
            }
        }
    }

    fn toggle_chat_batch_call(
        &mut self,
        pane_id: usize,
        index: usize,
        call: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let in_worker = pane.chat.worker_view.is_some();
        let messages = if let Some(view) = &mut pane.chat.worker_view {
            &mut view.messages
        } else {
            pane.chat.visible_messages_mut()
        };
        let Some(message) = messages.get_mut(index) else {
            return;
        };
        let Some(tool) = Arc::make_mut(message).tool.as_mut() else {
            return;
        };
        let Some(expanded) = tool.expanded_calls.get_mut(call) else {
            return;
        };
        *expanded = !*expanded;
        let opened = *expanded;
        if let Some(view) = &pane.chat.worker_view {
            if opened {
                view.list_state.pause_following_tail();
            }
            view.invalidate_message_row(index);
            pane.chat.notify_worker_view(cx);
        } else if !in_worker {
            if opened {
                pane.chat.list_state.pause_following_tail();
            }
            pane.chat.invalidate_message_row(index);
        }
        cx.notify();
    }

    fn toggle_chat_tool_show_full(&mut self, pane_id: usize, index: usize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id) {
            if let Some(view) = &mut pane.chat.worker_view {
                if !view.full_output.remove(&index) {
                    view.full_output.insert(index);
                }
                if let Some(message) = view.messages.get_mut(index)
                    && message.tool.is_some()
                {
                    let message = Arc::make_mut(message);
                    if let Some(tool) = message.tool.as_mut() {
                        tool.show_full = !tool.show_full;
                        if tool.show_full {
                            view.list_state.pause_following_tail();
                        }
                    }
                }
                view.invalidate_message_row(index);
                pane.chat.notify_worker_view(cx);
                cx.notify();
                return;
            }
            let messages = pane.chat.visible_messages_mut();
            if let Some(message) = messages.get_mut(index)
                && message.tool.is_some()
            {
                let message = Arc::make_mut(message);
                if let Some(tool) = message.tool.as_mut() {
                    tool.show_full = !tool.show_full;
                    if tool.show_full {
                        pane.chat.list_state.pause_following_tail();
                    }
                    pane.chat.invalidate_message_row(index);
                    cx.notify();
                }
            }
        }
    }

    fn toggle_worked_turn(&mut self, pane_id: usize, start: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else { return; };
        if let Some(view) = &mut pane.chat.worker_view {
            let first_row = view.row_cache.borrow().rows.iter().position(|row| matches!(row, ChatRow::Worked { start: index, .. } if *index == start));
            let turns = view.expanded_turns.entry(view.conversation_key.clone()).or_default();
            if !turns.remove(&start) { turns.insert(start); }
            view.invalidate_message_row(start);
            let running = Self::worker_running(&pane.chat.messages, view);
            let new_count = Self::build_chat_rows_from(&view.list_source(running)).borrow().list_item_count();
            update_worked_turn_rows(&view.list_state, &view.list_rows, first_row, new_count);
            pane.chat.notify_worker_view(cx);
            cx.notify();
            return;
        }
        let first_row = pane.chat.row_cache.borrow().rows.iter().position(|row| matches!(row, ChatRow::Worked { start: index, .. } if *index == start));
        let turns = pane.chat.expanded_turns.entry(pane.chat.conversation_key.clone()).or_default();
        if !turns.remove(&start) { turns.insert(start); }
        pane.chat.invalidate_message_row(start);
        let new_count = Self::build_chat_rows(&pane.chat, pane.chat.visible_running()).borrow().list_item_count();
        update_worked_turn_rows(&pane.chat.list_state, &pane.chat.list_rows, first_row, new_count);
        cx.notify();
    }

    fn toggle_chat_tool_group(&mut self, pane_id: usize, start: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if let Some(view) = &mut pane.chat.worker_view {
            let groups = view
                .expanded_tool_groups
                .entry(view.conversation_key.clone())
                .or_default();
            if !groups.remove(&start) {
                groups.insert(start);
                view.list_state.pause_following_tail();
            }
            view.invalidate_message_row(start);
            pane.chat.notify_worker_view(cx);
            cx.notify();
            return;
        }
        let groups = pane
            .chat
            .expanded_tool_groups
            .entry(pane.chat.conversation_key.clone())
            .or_default();
        if !groups.remove(&start) {
            groups.insert(start);
            pane.chat.list_state.pause_following_tail();
        }
        pane.chat.invalidate_message_row(start);
        cx.notify();
    }

    fn set_reasoning_effort(&mut self, pane_id: usize, level: &str, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if !pane.chat.selected_model.as_ref().is_some_and(|model| {
            model
                .thinking_levels()
                .iter()
                .any(|supported| supported == level)
        }) {
            return;
        }
        if let Some(bridge) = pane.chat.bridge.as_mut()
            && let Err(error) = bridge.set_thinking_level(level)
        {
            pane.chat
                .messages
                .push(Arc::new(ChatMessage::new(ChatRole::Error, error)));
            cx.notify();
            return;
        }
        pane.chat.thinking_override = Some(level.to_owned());
        if let Some(model) = &pane.chat.selected_model {
            pane.chat
                .thinking_by_model
                .insert(agent_model_selector(model), level.to_owned());
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn toggle_fast_mode(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if !pane
            .chat
            .selected_model
            .as_ref()
            .is_some_and(agent_auth::AgentModel::supports_reasoning_effort)
        {
            return;
        }
        pane.chat.fast_mode = !pane.chat.fast_mode;
        pane.chat.thinking_override = None;
        if let Some(model) = &pane.chat.selected_model {
            pane.chat
                .thinking_by_model
                .remove(&agent_model_selector(model));
        }
        let level = pane.chat.mode_thinking_level().to_owned();
        if let Some(bridge) = pane.chat.bridge.as_mut()
            && let Err(error) = bridge.set_thinking_level(&level)
        {
            pane.chat
                .messages
                .push(Arc::new(ChatMessage::new(ChatRole::Error, error)));
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn switch_agent_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.agent_layout {
            self.settings.agent_layout = false;
            let target = self.active_session().and_then(|session| {
                self.one_mode_pane(session)
                    .and_then(|pane| {
                        if pane.one_mode {
                            pane.regular_focus
                        } else {
                            Some(pane.id)
                        }
                    })
                    .and_then(|id| {
                        session
                            .panes
                            .iter()
                            .find(|pane| pane.id == id && !pane.one_mode && !pane.tool_panel)
                    })
                    .or_else(|| {
                        session
                            .panes
                            .iter()
                            .find(|pane| !pane.one_mode && !pane.tool_panel)
                    })
                    .map(|pane| pane.id)
            });
            for pane in self
                .projects
                .iter_mut()
                .flat_map(|project| &mut project.sessions)
                .flat_map(|session| &mut session.panes)
            {
                if pane.chat.active && !pane.one_mode && !pane.tool_panel {
                    pane.legacy_chat = true;
                    Self::update_chat_mode(pane, false, window, cx);
                }
            }
            if let Some(id) = target {
                self.focus_pane(id, window, cx);
            }
        } else {
            self.settings.agent_layout = true;
            if let Some(source) = self.active_session().and_then(|session| {
                session
                    .panes
                    .iter()
                    .find(|pane| Some(pane.id) == self.focused_pane_id)
                    .or_else(|| session.panes.first())
                    .map(|pane| pane.id)
            }) {
                self.focus_one_chat(source, window, cx);
            }
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn focus_workspace_pane(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane_uses_agent_layout(id) {
            self.focus_one_chat(id, window, cx);
        } else {
            self.focus_pane(id, window, cx);
        }
    }

    fn focus_one_chat(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.pane_uses_agent_layout(source) {
            return;
        }
        let Some(&(project, session, _)) = self.pane_index.get(&source) else {
            return;
        };
        let screen = &self.projects[project].sessions[session];
        let source = self
            .pane_ref(source)
            .filter(|pane| pane.tool_panel)
            .and_then(|pane| pane.preview_owner)
            .unwrap_or(source);
        let regular = screen
            .panes
            .iter()
            .find(|pane| pane.id == source && !pane.one_mode && !pane.tool_panel)
            .or_else(|| {
                screen
                    .panes
                    .iter()
                    .find(|pane| !pane.one_mode && !pane.tool_panel)
            });
        let regular_id = regular.map(|pane| pane.id);
        let maximized = screen.maximized_pane_id;
        let legacy = |pane: &&TerminalPane| {
            pane.legacy_chat
                && pane.diff_preview.is_none()
                && pane.markdown_preview.is_none()
                && !pane.files_active
        };
        let existing = regular
            .filter(legacy)
            .or_else(|| {
                screen
                    .panes
                    .iter()
                    .find(|pane| pane.one_mode && !pane.tool_panel)
            })
            .or_else(|| screen.panes.iter().find(legacy))
            .map(|pane| pane.id);
        let id = if let Some(id) = existing {
            id
        } else if let Some(regular_id) = regular_id {
            let id = self.next_id + 1;
            self.split_pane(regular_id, gpui::Axis::Horizontal, window, cx);
            self.projects[project].sessions[session].maximized_pane_id = maximized;
            let Some(pane) = self.pane_mut(id) else {
                return;
            };
            pane.one_mode = true;
            id
        } else {
            return;
        };
        if let Some(pane) = self.pane_mut(id)
            && pane.one_mode
            && regular_id.is_some()
        {
            pane.regular_focus = regular_id;
        }
        if self
            .pane_ref(id)
            .is_some_and(|pane| pane.input_state.is_none())
        {
            self.create_pane_input(id, window, cx);
        }
        if !self.set_chat_mode(id, true, window, cx) {
            self.focus_pane(id, window, cx);
        }
    }

    fn set_chat_mode(
        &mut self,
        pane_id: usize,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let changed = self
            .pane_mut(pane_id)
            .is_some_and(|pane| Self::update_chat_mode(pane, active, window, cx));
        if !changed {
            return false;
        }
        self.focus_pane(pane_id, window, cx);
        if active && let Some(pane) = self.pane_ref(pane_id) {
            let dir = self.agent_working_directory(pane);
            self.schedule_git_refresh(dir, cx);
        }
        true
    }
}
