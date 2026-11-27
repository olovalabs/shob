impl AppState {
    fn start_agent_turn(
        pane: &mut TerminalPane,
        prompt: String,
        cwd: PathBuf,
        save_history: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        pane.chat.retry_prompt = Some(prompt.clone());
        if !cwd.is_dir() {
            pane.chat.startup_error =
                Some(format!("Project folder unavailable: {}", cwd.display()));
            cx.notify();
            return false;
        }
        if pane.chat.selected_model.is_none() {
            pane.chat.startup_error = Some("Select a model to start chatting.".into());
            cx.notify();
            return false;
        }
        // A submitted turn (including a startup error) belongs at the live end.
        pane.chat.list_state.set_follow_mode(FollowMode::Tail);
        pane.chat.startup_error = None;
        if save_history && pane.chat.conversation.is_none() {
            pane.chat.conversation = Some(conversations::Log::open(
                &pane.uid,
                &pane.chat.conversation_key,
            ));
        }
        if let Err(error) = pane.chat.ensure_agent(cwd) {
            pane.chat.recovery = Some(error.clone());
            pane.chat.status = SessionStatus::Error;
            pane.chat
                .messages
                .push(Arc::new(ChatMessage::new(ChatRole::Error, error)));
            cx.notify();
            return false;
        }
        // Capabilities may have loaded after a bridge was warmed. Queue the
        // effective choice before every prompt so the displayed tier is used.
        if pane
            .chat
            .selected_model
            .as_ref()
            .is_some_and(agent_auth::AgentModel::supports_reasoning_effort)
        {
            let level = pane.chat.effective_thinking_level();
            if let Err(error) = pane
                .chat
                .bridge
                .as_ref()
                .unwrap()
                .set_thinking_level(&level)
            {
                pane.chat.recovery = Some(error.clone());
                pane.chat.status = SessionStatus::Error;
                pane.chat
                    .messages
                    .push(Arc::new(ChatMessage::new(ChatRole::Error, error)));
                cx.notify();
                return false;
            }
        }
        let trace = pane.chat.bridge.as_ref().map(|bridge| bridge.trace_id());
        let warm = pane.chat.completed_turns > 0 && !pane.chat.bridge_needs_context;
        if let Some(trace) = trace {
            crate::stream_timing::turn_sent(trace, warm);
        }
        let context = if pane.chat.bridge_needs_context {
            pane.chat.context_prompt(&prompt)
        } else {
            None
        };
        let resume = pane.chat.bridge_resume.as_deref();
        let needs_title = pane.chat.conversation_title().is_none()
            && pane.chat.first_response_provider.is_none()
            && !pane
                .chat
                .messages
                .iter()
                .any(|message| message.role == ChatRole::User);
        let bridge = pane.chat.bridge.as_mut().unwrap();
        let submitted_at = chat_timestamp_ms();
        let sent = if needs_title {
            bridge.send_first_prompt(
                &prompt,
                resume,
                context,
                pane.chat.cwd.as_deref().unwrap(),
                &agent_model_selector(pane.chat.selected_model.as_ref().unwrap()),
            )
        } else {
            bridge.send_prompt_with_context(&prompt, resume, context)
        };
        let sent = match sent {
            Ok(()) => {
                if needs_title && let Some(title) = agent_title_from_prompt(&prompt) {
                    pane.chat.set_prompt_title(title);
                }
                pane.chat.bridge_needs_context = false;
                if let Some(log) = pane.chat.conversation.as_mut() {
                    log.append_user_at(&prompt, submitted_at);
                }
                let mut message = ChatMessage::new(ChatRole::User, prompt);
                message.source_timestamp = submitted_at;
                pane.chat.messages.push(Arc::new(message));
                pane.chat.checklist = AgentChecklist::default();
                pane.chat.running = true;
                pane.chat.status = SessionStatus::Working;
                pane.chat.recovery = None;
                pane.chat.follow_ups_paused = false;
                true
            }
            Err(error) => {
                pane.chat.recovery = Some(error.clone());
                pane.chat.status = SessionStatus::Error;
                pane.chat
                    .messages
                    .push(Arc::new(ChatMessage::new(ChatRole::Error, error)));
                false
            }
        };
        cx.notify();
        sent
    }

    fn send_queued_follow_up(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        if pane.chat.running {
            return;
        }
        let cwd = self.agent_working_directory(pane);
        let save_history = self.settings.restore.agent_history;
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if let Some(prompt) = pane.chat.follow_ups.front().cloned() {
            if Self::start_agent_turn(pane, prompt, cwd, save_history, cx) {
                pane.chat.follow_ups.pop_front();
                self.schedule_workspace_save(cx);
            } else {
                pane.chat.follow_ups_paused = true;
            }
        }
    }

    fn execute_pane_input(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
            self.submit_agent_prompt(pane_id, window, cx);
            return;
        }
        let target = cx.entity().downgrade();
        let handle = window.window_handle();
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if pane.input_submitting {
            return;
        }
        let Some(input) = pane.input_state.clone() else {
            return;
        };
        let raw = input.read(cx).value().to_string();
        if raw.is_empty() && pane.attachments.is_empty() {
            pane.terminal
                .update(cx, |term, cx| term.send_bytes(b"\r", cx));
            return;
        }
        let submitted_files: Vec<PathBuf> = pane
            .attachments
            .iter()
            .map(|attachment| attachment.path.clone())
            .collect();
        let paths: Vec<String> = submitted_files
            .iter()
            .map(|path| attachments::prompt_path(path))
            .collect();
        let terminal = pane.terminal.downgrade();
        let generation = pane.generation.clone();
        pane.input_submitting = true;
        let (cancel, cancelled) = smol::channel::bounded(1);
        pane.input_cancel = Some(cancel);
        input.update(cx, |state, cx| state.focus(window, cx));
        App::spawn(cx, async move |cx: &mut gpui::AsyncApp| {
            // Timers begin after physical delivery, never merely after enqueue.
            let delivery = async {
                if paths.is_empty() {
                    let receipt = terminal
                        .update(cx, |term, cx| term.send_text_with_completion(&raw, cx))
                        .map_err(|error| error.to_string())??;
                    receipt.wait().await?;
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(25))
                        .await;
                } else {
                    if !raw.is_empty() {
                        let receipt = terminal
                            .update(cx, |term, cx| {
                                term.send_text_with_completion(&format!("{raw} "), cx)
                            })
                            .map_err(|error| error.to_string())??;
                        receipt.wait().await?;
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(30))
                            .await;
                    }
                    for path in &paths {
                        let receipt = terminal
                            .update(cx, |term, cx| {
                                term.paste_text_from_app_with_completion(path, cx)
                            })
                            .map_err(|error| error.to_string())??;
                        receipt.wait().await?;
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(100))
                            .await;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(300))
                        .await;
                }
                let receipt = terminal
                    .update(cx, |term, cx| term.send_bytes_with_completion(b"\r", cx))
                    .map_err(|error| error.to_string())??;
                receipt.wait().await
            };
            let delivered: std::result::Result<(), String> = smol::future::race(delivery, async {
                let _ = cancelled.recv().await;
                Err("PTY input submission cancelled; accepted bytes are not replayed".into())
            })
            .await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = target.update(cx, |state, cx| {
                    let Some(pane) = state
                        .pane_mut(pane_id)
                        .filter(|pane| pane.generation == generation)
                    else {
                        return;
                    };
                    pane.input_submitting = false;
                    pane.input_cancel = None;
                    match delivered {
                        Ok(()) => {
                            if !raw.is_empty() && pane.command_history.last() != Some(&raw) {
                                pane.command_history.push(raw.clone());
                            }
                            pane.history_index = None;
                            // Do not erase anything typed/attached during delivery.
                            pane.attachments
                                .retain(|attachment| !submitted_files.contains(&attachment.path));
                            if input.read(cx).value().as_ref() == raw.as_str() {
                                pane.pending_draft.clear();
                                input.update(cx, |input, cx| input.set_value("", window, cx));
                            }
                        }
                        Err(error) => {
                            pane.terminal
                                .update(cx, |term, cx| term.report_io_error(error, cx));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// Transfer input ownership between the editor and the command's PTY.
    fn on_shell_event(&mut self, pane_id: usize, event: &ShellEvent, cx: &mut Context<Self>) {
        let mut cwd_changed = false;
        {
            let Some(pane) = self.pane_mut(pane_id) else {
                return;
            };
            match event {
                ShellEvent::CommandStarted { .. } => {
                    pane.command_running = true;
                    pane.completion_menu = None;
                }
                ShellEvent::CommandFinished { .. } | ShellEvent::PromptStart => {
                    pane.command_running = false;
                    pane.bottom_input_visible = true;
                }
                ShellEvent::CwdChanged(path) => {
                    pane.cwd = Some(PathBuf::from(path));
                    cwd_changed = true;
                }
                _ => return,
            }
            let show_input = pane.should_show_bottom_input(cx);
            pane.terminal
                .update(cx, |terminal, _| terminal.set_read_only(show_input));
        }
        if cwd_changed {
            self.mark_dirty(cx);
        }
        cx.notify();
    }

    fn handle_pane_ctrl_c(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .pane_ref(pane_id)
            .is_some_and(|pane| pane.chat.worker_view.is_some())
        {
            return;
        }
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
        if pane.chat.active {
            if pane.chat.running {
                pane.chat.follow_ups_paused = true;
                pane.chat.requests.clear();
                let trace = pane.chat.bridge.as_ref().map(|bridge| bridge.trace_id());
                if let Some(bridge) = pane.chat.bridge.as_mut()
                    && let Err(error) = bridge.cancel()
                {
                    pane.chat
                        .messages
                        .push(Arc::new(ChatMessage::new(ChatRole::Error, error)));
                }
                if let Some(trace) = trace {
                    crate::stream_timing::turn_cancelled(trace);
                }
            } else if let Some(input) = &pane.input_state {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            cx.notify();
            return;
        }

        if let Some(cancel) = &pane.input_cancel {
            let _ = cancel.try_send(());
        }

        // 1. Clear input state and draft
        if let Some(input_state) = &pane.input_state {
            input_state.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        pane.history_index = None;
        pane.pending_draft.clear();
        pane.completion_menu = None;

        // 2. Send \x03 (ETX/SIGINT) to PTY (interrupts process on Linux, macOS, and Windows ConPTY)
        pane.terminal.update(cx, |term, cx| {
            term.send_bytes(b"\x03", cx);
        });

        // 3. On Unix: also directly signal the active foreground process group
        #[cfg(unix)]
        if let Some(pid) = pane.pid {
            smol::spawn(smol::unblock(move || {
                if let Some(tpgid) = status::read_proc_tpgid(pid)
                    && tpgid > 0
                    && tpgid != pid as i64
                {
                    unsafe {
                        libc::kill(-(tpgid as libc::pid_t), libc::SIGINT);
                    }
                }
            }))
            .detach();
        }

        // 4. If an agent/command was working, interrupt and return to Idle
        if pane.status == SessionStatus::Working {
            pane.status = SessionStatus::Idle;
            pane.bottom_input_visible = true;
            pane.terminal.update(cx, |term, _| term.set_read_only(true));
        }

        cx.notify();
    }

    fn handle_pane_input_key(
        &mut self,
        pane_id: usize,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let key = keystroke.key.as_str();
        let modifiers = keystroke.modifiers;

        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        if pane.chat.auth.is_some() {
            if event.keystroke.key == "escape" {
                cx.stop_propagation();
                self.close_agent_auth(pane_id, window, cx);
            }
            return;
        }
        let Some(input_state) = pane.input_state.clone() else {
            return;
        };
        let value = input_state.read(cx).value().to_string();
        let cursor_utf16 = input_state.read(cx).cursor();
        let cursor_byte = completion::utf16_to_byte(&value, cursor_utf16);
        let cursor_line = input_state.read(cx).cursor_position().line;
        let last_line = value.matches('\n').count();
        let has_menu = pane.completion_menu.is_some();
        let plus_menu_open = pane.plus_menu_open;
        let terminal = pane.terminal.clone();

        // --- completion menu navigation ------------------------------------
        if has_menu {
            if key == "down" {
                cx.stop_propagation();
                self.move_completion_selection(pane_id, 1, cx);
                return;
            }
            if key == "up" {
                cx.stop_propagation();
                self.move_completion_selection(pane_id, -1, cx);
                return;
            }
            if key == "right" && !modifiers.control && !modifiers.shift && !modifiers.alt {
                cx.stop_propagation();
                let selected = self
                    .pane_ref(pane_id)
                    .and_then(|pane| pane.completion_menu.as_ref().map(|menu| menu.selected))
                    .unwrap_or(0);
                self.accept_completion_at(pane_id, selected, window, cx);
                return;
            }
            if (key == "enter" || key == "return") && !modifiers.shift && pane.chat.active {
                cx.stop_propagation();
                let selected = pane
                    .completion_menu
                    .as_ref()
                    .map_or(0, |menu| menu.selected);
                self.accept_completion_at(pane_id, selected, window, cx);
                return;
            }
            if (key == "enter" || key == "return") && !modifiers.shift {
                // Enter means "run this", never "insert the highlighted
                // suggestion": Tab and Right accept completions.
                cx.stop_propagation();
                self.close_completion_menu(pane_id, cx);
                self.execute_pane_input(pane_id, window, cx);
                return;
            }
            if key == "tab" && !modifiers.control && !modifiers.alt {
                cx.stop_propagation();
                let selected = self
                    .pane_ref(pane_id)
                    .and_then(|pane| pane.completion_menu.as_ref().map(|menu| menu.selected))
                    .unwrap_or(0);
                self.accept_completion_at(pane_id, selected, window, cx);
                return;
            }
            if key == "escape" {
                cx.stop_propagation();
                self.close_completion_menu(pane_id, cx);
                return;
            }
        }

        // Enter: execute the command.
        if (key == "enter" || key == "return")
            && !modifiers.shift
            && !modifiers.control
            && !modifiers.alt
        {
            cx.stop_propagation();
            self.execute_pane_input(pane_id, window, cx);
            return;
        }

        // Shift+Enter: insert a newline (multi-line commands, Shob style).
        if (key == "enter" || key == "return")
            && modifiers.shift
            && !modifiers.control
            && !modifiers.alt
        {
            input_state.update(cx, |state, cx| {
                state.insert("\n", window, cx);
            });
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+C is handled by the terminal's `Copy` action as well; the guard
        // here keeps it working even when the editor has no selection.
        if key == "c" && modifiers.control && !modifiers.shift && !modifiers.alt {
            cx.stop_propagation();
            self.handle_pane_ctrl_c(pane_id, window, cx);
            return;
        }
        if self.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
            return;
        }

        // Ctrl+R: reverse history search, like the shell's own prompt.
        if key == "r" && modifiers.control && !modifiers.shift && !modifiers.alt {
            cx.stop_propagation();
            self.open_history_search(pane_id, window, cx);
            return;
        }

        // Tab: accept the ghost suggestion, or open completion candidates.
        if key == "tab" && !modifiers.control && !modifiers.alt {
            cx.stop_propagation();
            self.start_completion(pane_id, window, cx);
            return;
        }

        // Right at the end of the line accepts a ghost suggestion.
        if key == "right"
            && !modifiers.control
            && !modifiers.shift
            && !modifiers.alt
            && cursor_utf16 >= completion::utf16_len(&value)
            && input_state.read(cx).has_inline_completion()
        {
            input_state.update(cx, |state, cx| {
                state.accept_inline_completion(window, cx);
            });
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+A / Ctrl+E: readline start/end of line.
        if key == "a" && modifiers.control && !modifiers.shift && !modifiers.alt {
            let start = value[..cursor_byte].rfind('\n').map_or(0, |idx| idx + 1);
            move_input_cursor(
                &input_state,
                completion::byte_to_utf16(&value, start),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "e" && modifiers.control && !modifiers.shift && !modifiers.alt {
            let end = value[cursor_byte..]
                .find('\n')
                .map_or(value.len(), |idx| cursor_byte + idx);
            move_input_cursor(
                &input_state,
                completion::byte_to_utf16(&value, end),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+U: kill from the start of the line to the cursor.
        if key == "u" && modifiers.control && !modifiers.shift && !modifiers.alt {
            let start = value[..cursor_byte].rfind('\n').map_or(0, |idx| idx + 1);
            let mut new_value = String::with_capacity(value.len());
            new_value.push_str(&value[..start]);
            new_value.push_str(&value[cursor_byte..]);
            set_input_text_with_cursor(
                &input_state,
                new_value,
                completion::byte_to_utf16(&value, start),
                window,
                cx,
            );
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.history_index = None;
                pane.pending_draft.clear();
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+W / Alt+Backspace: delete the previous word.
        if (key == "w" && modifiers.control && !modifiers.shift && !modifiers.alt)
            || (key == "backspace" && modifiers.alt && !modifiers.control && !modifiers.shift)
        {
            let start = previous_word_start(&value, cursor_byte);
            let mut new_value = String::with_capacity(value.len());
            new_value.push_str(&value[..start]);
            new_value.push_str(&value[cursor_byte..]);
            set_input_text_with_cursor(
                &input_state,
                new_value,
                completion::byte_to_utf16(&value, start),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+K: kill from the cursor to the end of the line.
        if key == "k" && modifiers.control && !modifiers.shift && !modifiers.alt {
            let end = value[cursor_byte..]
                .find('\n')
                .map_or(value.len(), |idx| cursor_byte + idx);
            let mut new_value = String::with_capacity(value.len());
            new_value.push_str(&value[..cursor_byte]);
            new_value.push_str(&value[end..]);
            set_input_text_with_cursor(&input_state, new_value, cursor_utf16, window, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Alt+B / Alt+F: move by word (readline).
        if key == "b" && modifiers.alt && !modifiers.control && !modifiers.shift {
            let start = previous_word_start(&value, cursor_byte);
            move_input_cursor(
                &input_state,
                completion::byte_to_utf16(&value, start),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "f" && modifiers.alt && !modifiers.control && !modifiers.shift {
            let end = next_word_end(&value, cursor_byte);
            move_input_cursor(
                &input_state,
                completion::byte_to_utf16(&value, end),
                window,
                cx,
            );
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+L: clear the terminal screen.
        if key == "l" && modifiers.control && !modifiers.shift && !modifiers.alt {
            terminal.update(cx, |term, cx| {
                term.send_bytes(b"\x0c", cx);
            });
            cx.stop_propagation();
            return;
        }

        // Ctrl+Z stays with the editor (undo), not job control: a terminal
        // prompt's line editor is what the user is typing into here.

        // Ctrl+D: EOF on an empty line.
        if key == "d" && modifiers.control && !modifiers.shift && !modifiers.alt && value.is_empty()
        {
            terminal.update(cx, |term, cx| {
                term.send_bytes(b"\x04", cx);
            });
            cx.stop_propagation();
            return;
        }

        // Ctrl+\: quit the foreground job (SIGQUIT).
        if key == "\\" && modifiers.control && !modifiers.shift && !modifiers.alt {
            terminal.update(cx, |term, cx| {
                term.send_bytes(b"\x1c", cx);
            });
            cx.stop_propagation();
            return;
        }

        // Escape: dismiss menus, then clear the input.
        if key == "escape" && !modifiers.shift && !modifiers.control && !modifiers.alt {
            if self.terminal_context_menu.is_some() {
                self.terminal_context_menu = None;
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if plus_menu_open {
                if let Some(pane) = self.pane_mut(pane_id) {
                    pane.plus_menu_open = false;
                }
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if !value.is_empty() {
                input_state.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                });
                if let Some(pane) = self.pane_mut(pane_id) {
                    pane.history_index = None;
                    pane.pending_draft.clear();
                }
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }

        // Ctrl+P / Ctrl+N: previous / next history entry.
        if (key == "p" || key == "n") && modifiers.control && !modifiers.shift && !modifiers.alt {
            cx.stop_propagation();
            self.step_pane_history(pane_id, key == "p", window, cx);
            return;
        }

        // Up / Down: history, but only once the cursor is on the first / last
        // line of a multi-line buffer, so line navigation still works.
        if key == "up" && !modifiers.shift && !modifiers.control && !modifiers.alt {
            if cursor_line == 0 {
                cx.stop_propagation();
                self.step_pane_history(pane_id, true, window, cx);
            }
            return;
        }
        if key == "down" && !modifiers.shift && !modifiers.control && !modifiers.alt {
            if cursor_line as usize >= last_line {
                cx.stop_propagation();
                self.step_pane_history(pane_id, false, window, cx);
            }
            return;
        }

        // Ordinary typing leaves history navigation mode.
        if (!modifiers.control && !modifiers.alt)
            && (keystroke.key_char.is_some() || key == "backspace" || key == "delete")
            && let Some(pane) = self.pane_mut(pane_id)
        {
            pane.history_index = None;
        }
    }

    fn ensure_panes_input_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut files = Vec::new();
        let mut previews = Vec::new();
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    if let Some(path) = pane.tool_panel_file_restore.take() {
                        previews.push((pane.id, project.path.clone(), path));
                    }
                    if let Some(saved) = pane.files_restore.take() {
                        files.push((pane.id, project.path.clone(), saved));
                    }
                }
            }
        }
        for (id, root, saved) in files {
            let view = self.build_files_surface(id, root, saved, window, cx);
            if let Some(pane) = self.pane_mut(id) {
                pane.files_surface = Some(view);
            }
        }
        for (id, root, path) in previews {
            if file_preview::is_markdown_path(&path) {
                self.load_markdown_preview(id, path, cx);
            } else {
                let display_path = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                self.show_read_only_file(id, root, display_path, window, cx);
            }
        }
        let mut missing = Vec::new();
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    let placeholder = if pane.chat.active {
                        "Message the agent…"
                    } else {
                        "Run commands"
                    };
                    if let Some(input) = &pane.input_state {
                        input.update(cx, |state, cx| {
                            state.set_placeholder(placeholder, window, cx);
                            state.set_auto_grow(1, 8);
                        });
                        if pane.input_subscription.is_none() && pane.input_state.is_none() {
                            missing.push(pane.id);
                        }
                    } else {
                        missing.push(pane.id);
                    }
                }
            }
        }
        for pane_id in missing {
            self.create_pane_input(pane_id, window, cx);
        }

        // Workspace input must not steal focus from a transient modal.
        if self.active_view == ActiveView::Workspace
            && !self.conversation_header_menu_focus.contains_focused(window, cx)
            && self.history_search.is_none()
            && !self.sidebar_search_focused(window, cx)
            && !self.sidebar_project_picker_focused(window, cx)
            && let Some(pane_id) = self.focused_pane_id
            && let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id)
            && let Some(project) = self.projects.get(p_idx)
            && let Some(session) = project.sessions.get(s_idx)
            && let Some(pane) = session.panes.get(pane_idx)
        {
            if pane.files_active
                || pane.chat.auth.is_some()
                || pane.branch_picker.is_some()
                || pane.chat.active && !pane.chat.requests.is_empty()
                || (self.settings.agent_layout
                    && !pane.tool_panel_hidden
                    && pane
                        .tool_panel_chooser
                        .as_ref()
                        .is_some_and(|focus| focus.is_focused(window)))
            {
                return;
            }
            let should_show = pane.should_show_bottom_input(cx);
            pane.terminal.update(cx, |term, _| {
                term.set_read_only(should_show);
            });
            if should_show {
                if let Some(input_state) = &pane.input_state {
                    let is_input_focused = input_state.read(cx).focus_handle().is_focused(window);
                    let is_term_focused = pane.terminal.read(cx).focus_handle().is_focused(window);
                    if is_term_focused || !is_input_focused {
                        input_state.update(cx, |state, cx| {
                            state.focus(window, cx);
                        });
                    }
                }
            } else {
                let is_term_focused = pane.terminal.read(cx).focus_handle().is_focused(window);
                if !is_term_focused {
                    pane.terminal
                        .read(cx)
                        .focus_handle()
                        .clone()
                        .focus(window, cx);
                }
            }
        }
    }
}
