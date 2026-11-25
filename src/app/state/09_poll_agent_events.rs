impl AppState {
    fn poll_agent_events(&mut self, cx: &mut Context<Self>) -> bool {
        let pump_start = crate::stream_timing::enabled().then(std::time::Instant::now);
        let mut changed = false;
        let mut session_changed = false;
        let mut authenticated = false;
        let mut streaming = false;
        let mut titled_panes: Vec<usize> = Vec::new();
        let mut done_panes: Vec<usize> = Vec::new();
        let mut ready_follow_ups = Vec::new();
        let mut removed_providers = Vec::new();
        let save_history = self.settings.restore.agent_history;
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    if let Some(auth) = pane.chat.auth.as_mut() {
                        if let Some(receiver) = &auth.provider_operation {
                            let result = match receiver.try_recv() {
                                Ok(result) => Some(result),
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    Some(Err("Provider operation stopped unexpectedly.".into()))
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                            };
                            if let Some(result) = result {
                                auth.provider_operation = None;
                                match result {
                                    Ok(removed) => {
                                        auth.custom_provider = None;
                                        auth.selected = None;
                                        if let Some(provider) = removed {
                                            removed_providers.push(provider);
                                        }
                                        pane.chat.auth_refresh_pending = pane.chat.bridge.is_some();
                                        auth.discover();
                                    }
                                    Err(error) => auth.error = Some(error),
                                }
                                changed = true;
                            }
                        }
                        if let Some(receiver) = &auth.discovery {
                            match receiver.try_recv() {
                                Ok(result) => {
                                    auth.discovery = None;
                                    match result {
                                        Ok(providers) => auth.providers = providers,
                                        Err(error) => auth.error = Some(error),
                                    }
                                    changed = true;
                                }
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    auth.discovery = None;
                                    auth.error =
                                        Some("Provider discovery stopped unexpectedly.".into());
                                    changed = true;
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                            }
                        }
                        if let Some(receiver) = &auth.model_discovery {
                            match receiver.try_recv() {
                                Ok(result) => {
                                    auth.model_discovery = None;
                                    match result {
                                        Ok(models) => {
                                            auth.models = models;
                                            auth.model_error = None;
                                        }
                                        Err(error) => auth.model_error = Some(error),
                                    }
                                    changed = true;
                                }
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    auth.model_discovery = None;
                                    auth.model_error =
                                        Some("Model discovery stopped unexpectedly.".into());
                                    changed = true;
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                            }
                        }
                        let mut finished = None;
                        if let Some(login) = auth.login.as_mut() {
                            while let Some(event) = login.try_event() {
                                changed = true;
                                match event {
                                    agent_auth::LoginEvent::BrowserUrl(url) => {
                                        cx.open_url(&url);
                                        auth.output.push_str("Complete sign-in in your browser.");
                                    }
                                    agent_auth::LoginEvent::Output(text) => {
                                        auth.output.push_str(&text);
                                    }
                                    agent_auth::LoginEvent::Finished(result) => {
                                        finished = Some(result);
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(result) = finished {
                            auth.login = None;
                            match result {
                                Ok(()) => {
                                    authenticated = true;
                                    auth.output.push_str("\nAuthentication complete.");
                                    auth.discover();
                                }
                                Err(error) => auth.error = Some(error),
                            }
                        }
                    }
                    if let Some(mut bridge) = pane.chat.bridge.take() {
                        let mut content_changed = false;
                        let previous = pane.chat.requests.len();
                        pane.chat.requests.retain(agent_interaction::Request::alive);
                        pane.chat.requests.extend(bridge.poll_requests());
                        if !pane.chat.requests.is_empty() {
                            if pane.chat.status != SessionStatus::Blocked
                                || previous != pane.chat.requests.len()
                            {
                                pane.chat.status = SessionStatus::Blocked;
                                changed = true;
                            }
                        } else if previous != 0 {
                            if pane.chat.running && pane.chat.status == SessionStatus::Blocked {
                                pane.chat.status = SessionStatus::Working;
                            }
                            changed = true;
                        }
                        let mut closed = false;
                        let trace = bridge.trace_id();
                        let paint_turn = crate::stream_timing::turn_started_at(trace)
                            .map(|started| (trace, started));
                        let drain_start = std::time::Instant::now();
                        let drained = bridge.poll();
                        crate::stream_timing::record_since("ui.drain", drain_start);
                        if drained.iter().any(|event| !is_task_list_event(event)) {
                            pane.chat.active_dirty.set(true);
                            if drained.iter().any(|event| {
                                !is_task_list_event(event)
                                    && matches!(
                                        event,
                                        agent_bridge::AgentEvent::ToolStart { .. }
                                            | agent_bridge::AgentEvent::ToolUpdate { .. }
                                            | agent_bridge::AgentEvent::ToolEnd { .. }
                                            | agent_bridge::AgentEvent::Subagents { .. }
                                            | agent_bridge::AgentEvent::Done
                                            | agent_bridge::AgentEvent::Closed
                                    )
                            }) {
                                pane.chat.activity_dirty.set(true);
                            }
                        }
                        for event in drained {
                            let event_at = chat_timestamp_ms();
                            if is_task_list_event(&event) {
                                if let Some(list) = task_list_update(&event)
                                    && Self::apply_task_list(pane, list, cx)
                                {
                                    changed = true;
                                    if save_history
                                        && let Some(log) = pane.chat.conversation.as_mut()
                                    {
                                        log.append_event_at(&event, event_at);
                                    }
                                }
                                continue;
                            }
                            let apply_start = std::time::Instant::now();
                            changed |= PaneChat::event_updates_root(&event);
                            content_changed = true;
                            let provider_changed = pane.chat.record_first_response_provider(&event);
                            session_changed |= provider_changed;
                            changed |= provider_changed;
                            let delta_kind = match &event {
                                agent_bridge::AgentEvent::Thinking(text) if !text.is_empty() => {
                                    Some(crate::stream_timing::DeltaKind::Reasoning)
                                }
                                agent_bridge::AgentEvent::Text(text) if !text.is_empty() => {
                                    Some(crate::stream_timing::DeltaKind::Answer)
                                }
                                _ => None,
                            };
                            let startup_error = pane.chat.messages.is_empty()
                                && !pane.chat.running
                                && matches!(event, agent_bridge::AgentEvent::Error(_));
                            match &event {
                                agent_bridge::AgentEvent::Error(error) => {
                                    pane.chat.recovery = Some(error.clone());
                                    pane.chat.follow_ups_paused = true;
                                }
                                agent_bridge::AgentEvent::Done
                                | agent_bridge::AgentEvent::Closed => {
                                    pane.chat.requests.clear();
                                }
                                _ => {}
                            }
                            pane.chat.observe_status(&event);
                            if save_history
                                && !startup_error
                                && let Some(log) = pane.chat.conversation.as_mut()
                                && !matches!(
                                    &event,
                                    agent_bridge::AgentEvent::Title(_)
                                        | agent_bridge::AgentEvent::Done
                                        | agent_bridge::AgentEvent::Closed
                                )
                            {
                                log.append_event_at(&event, event_at);
                            }
                            match event {
                                agent_bridge::AgentEvent::Done => {
                                    pane.chat.completed_turns =
                                        pane.chat.completed_turns.saturating_add(1);
                                    crate::stream_timing::turn_settled(trace);
                                    if let Some(index) = finish_worked_turn(&mut pane.chat.messages, event_at) {
                                        pane.chat.invalidate_message_row(index);
                                    }
                                    finish_chat_tools(&mut pane.chat.messages);
                                    finish_chat_streaming(&mut pane.chat.messages);
                                    done_panes.push(pane.id);
                                }
                                agent_bridge::AgentEvent::Closed => {
                                    pane.chat.follow_ups_paused = true;
                                    if pane.chat.status == SessionStatus::Error
                                        && pane.chat.recovery.is_none()
                                    {
                                        pane.chat.recovery =
                                            Some("The agent session closed unexpectedly.".into());
                                    }
                                    crate::stream_timing::turn_settled(trace);
                                    if let Some(index) = finish_worked_turn(&mut pane.chat.messages, event_at) {
                                        pane.chat.invalidate_message_row(index);
                                    }
                                    finish_chat_tools(&mut pane.chat.messages);
                                    finish_chat_streaming(&mut pane.chat.messages);
                                    closed = true;
                                }
                                agent_bridge::AgentEvent::Error(message) if startup_error => {
                                    pane.chat.startup_error = Some(message);
                                }
                                agent_bridge::AgentEvent::Title(title) => {
                                    pane.chat.set_agent_title(title);
                                    titled_panes.push(pane.id);
                                    session_changed = true;
                                }
                                other => push_chat_event(&mut pane.chat.messages, other),
                            }
                            if let Some(kind) = delta_kind {
                                crate::stream_timing::turn_delta_applied(trace, kind);
                                if let Some(last) = pane.chat.messages.last_mut() {
                                    Arc::make_mut(last).turn = paint_turn;
                                }
                            }
                            crate::stream_timing::record_since("ui.apply", apply_start);
                        }
                        if content_changed {
                            pane.chat.notify_content(cx);
                        }
                        if !pane.chat.bridge_needs_context
                            && let Some(id) = bridge.session_id()
                            && pane.chat.shob_session_id.as_ref() != Some(&id)
                        {
                            session_changed = true;
                            changed = true;
                            pane.chat.shob_session_id = Some(id);
                        }
                        if closed {
                            pane.chat.cwd = None;
                        } else {
                            pane.chat.bridge = Some(bridge);
                        }
                    }
                    if let Some(error) = pane
                        .chat
                        .conversation
                        .as_mut()
                        .and_then(conversations::Log::take_error)
                    {
                        // Persistence failures are visible, but never logged recursively.
                        pane.chat
                            .messages
                            .push(Arc::new(ChatMessage::new(ChatRole::Error, error)));
                        pane.chat.mark_active_dirty();
                        changed = true;
                    }
                    if !pane.chat.running
                        && !pane.chat.follow_ups_paused
                        && !pane.chat.follow_ups.is_empty()
                    {
                        ready_follow_ups.push(pane.id);
                    }
                    streaming |= pane.chat.running;
                }
            }
        }
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    if authenticated && pane.chat.bridge.is_some() {
                        pane.chat.auth_refresh_pending = true;
                    }
                    if pane.chat.auth_refresh_pending && !pane.chat.running {
                        let mut retry = false;
                        let thinking = pane
                            .chat
                            .bridge_model
                            .as_ref()
                            .filter(|model| model.supports_reasoning_effort())
                            .map(|_| pane.chat.effective_thinking_level());
                        if let Some(bridge) = pane.chat.bridge.as_mut() {
                            let result = bridge.restart_after_auth().and_then(|()| {
                                thinking
                                    .as_deref()
                                    .map_or(Ok(()), |level| bridge.set_thinking_level(level))
                            });
                            match result {
                                Ok(()) => {}
                                // The embedded harness is still opening its
                                // session; retry on the next tick instead of
                                // surfacing a spurious error.
                                Err(error) if error.contains("still starting") => retry = true,
                                Err(error) => pane
                                    .chat
                                    .messages
                                    .push(Arc::new(ChatMessage::new(ChatRole::Error, error))),
                            }
                        }
                        if !retry {
                            pane.chat.auth_refresh_pending = false;
                        }
                        changed = true;
                    }
                }
            }
        }
        if !removed_providers.is_empty() {
            if self
                .default_agent_model
                .as_ref()
                .is_some_and(|model| removed_providers.contains(&model.provider))
            {
                self.default_agent_model = None;
            }
            for project in &mut self.projects {
                for session in &mut project.sessions {
                    for pane in &mut session.panes {
                        if pane
                            .chat
                            .selected_model
                            .as_ref()
                            .is_some_and(|model| removed_providers.contains(&model.provider))
                        {
                            pane.chat.selected_model = None;
                        }
                    }
                }
            }
            self.schedule_workspace_save(cx);
        }
        for pane_id in ready_follow_ups {
            self.send_queued_follow_up(pane_id, cx);
        }
        for pane_id in titled_panes {
            self.sync_session_title(pane_id);
        }
        for pane_id in done_panes {
            if let Some(pane) = self.pane_ref(pane_id) {
                let dir = self.agent_working_directory(pane);
                git_workspace::invalidate_branch_cache(&dir);
                self.schedule_git_refresh(dir, cx);
            }
        }
        if session_changed {
            self.schedule_workspace_save(cx);
        }
        if let Some(start) = pump_start {
            crate::stream_timing::record_since("ui.pump", start);
        }
        if changed {
            cx.notify();
        }
        streaming
    }

    /// Per-pane memory and lifecycle totals, for the `SHOB_DIAGNOSTICS` line.
    ///
    /// Every field is an estimate of a named structure, not an allocator
    /// measurement. That is enough to rank consumers and to prove what
    /// hibernation actually gave back.
    fn memory_report(&self, cx: &App) -> session_lifecycle::WorkspaceMemory {
        let mut totals = session_lifecycle::WorkspaceMemory::default();
        for project in &self.projects {
            for session in &project.sessions {
                for pane in &session.panes {
                    let terminal = pane.terminal.read(cx);
                    let view = terminal.memory_usage();
                    let mut entities = pane.chat.retained_entities();
                    if pane.input_state.is_some() {
                        entities += 1;
                    }
                    if pane.files_surface.is_some() {
                        entities += 1;
                    }
                    let mut tasks = 0;
                    if pane.terminal_subscription.is_some() {
                        tasks += 1;
                    }
                    if pane.input_subscription.is_some() {
                        tasks += 1;
                    }
                    let mut threads = terminal.thread_count();
                    if pane.chat.bridge.is_some() {
                        // The embedded backend owns its own runtime; count the
                        // handle as one provider thread rather than guessing
                        // the runtime's worker count.
                        tasks += 1;
                        threads += 1;
                    }
                    totals.record(
                        pane.lifecycle.state(),
                        &session_lifecycle::MemoryUsage {
                            grid_bytes: view.grid.grid_bytes,
                            grid_reserved_bytes: view.grid.reserved_bytes,
                            hibernated_text_bytes: view.hibernated_text_bytes,
                            backlog_bytes: view.backlog_bytes,
                            view_cache_bytes: view.renderer_bytes + pane.chat.row_cache_bytes(),
                            message_bytes: pane.chat.message_bytes(),
                            entities,
                            tasks,
                            threads,
                        },
                    );
                }
            }
        }
        totals
    }

    /// Reclaim what an unselected pane does not need.
    ///
    /// Two levels, both reversible without touching the shell:
    /// 1. every idle pane releases render caches and any releasable agent
    ///    bridge (the pre-existing behaviour);
    /// 2. a pane that has been quiet for the configured threshold and has no
    ///    work in flight drops its terminal grid, keeping the PTY, a bounded
    ///    text snapshot and a bounded backlog of whatever arrives while asleep.
    ///
    /// Everything expensive about waking is a render: the grid is rebuilt from
    /// the kept text, so returning to a session never waits on the disk.
    fn release_inactive_resources(&mut self, cx: &mut Context<Self>) -> bool {
        let selected = self.active_session().map(|session| session.id);
        let policy = self.settings.memory.policy();
        let now = std::time::Instant::now();
        let mut session_changed = false;
        // A deep scrollback is thousands of small row allocations, and its drop
        // runs on this thread. Hibernating one pane per tick keeps the freed
        // total bounded instead of stalling a restored 50-session workspace.
        let mut hibernated_this_tick = false;
        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    let visible = Some(session.id) == selected;
                    let (terminal_hibernated, alt_screen) = {
                        let terminal = pane.terminal.read(cx);
                        (terminal.is_hibernated(), terminal.is_alt_screen())
                    };
                    if visible || pane.has_live_work() {
                        pane.idle_resources_released = false;
                        if visible {
                            pane.lifecycle.mark_seen(now);
                            if pane.lifecycle.state().is_hibernated() {
                                pane.lifecycle.woke(now);
                            }
                        } else {
                            pane.lifecycle.touch(now);
                        }
                        continue;
                    }
                    // Hibernation is only for a quiet shell prompt. A
                    // foreground command (`command_running`, which covers an
                    // agent CLI that is mid-turn), a full-screen program, or a
                    // preview pane keeps its grid: those repaint on their own
                    // schedule, so an emptied grid would not come back by
                    // itself, and no process is ever stalled or killed behind
                    // the person's back.
                    let quiescent = !terminal_hibernated
                        && !pane.command_running
                        && pane.preview_owner.is_none()
                        && !pane.tool_panel
                        && pane.markdown_preview.is_none()
                        && pane.diff_preview.is_none()
                        && !alt_screen;
                    let allowed = quiescent && !hibernated_this_tick;
                    let transition = pane.lifecycle.evaluate(now, policy, allowed);
                    if allowed && transition == session_lifecycle::Transition::ToHibernated {
                        let text = pane.terminal.update(cx, |terminal, _| terminal.hibernate());
                        if let Some(text) = text {
                            hibernated_this_tick = true;
                            pane.idle_resources_released = true;
                            pane.poll_cache = None;
                            pane.screen_scratch = Vec::new();
                            pane.chat.view.borrow_mut().take();
                            pane.chat.focused_scrollbar.borrow_mut().take();
                            let rows = std::mem::take(&mut *pane.chat.row_cache.borrow_mut());
                            let uid = pane.uid.clone();
                            cx.background_executor()
                                .spawn(async move {
                                    Self::persist_hibernated_scrollback(&uid, &text);
                                    drop(rows);
                                    session_lifecycle::return_free_pages();
                                })
                                .detach();
                            continue;
                        }
                    }
                    let bridge = if pane
                        .chat
                        .bridge
                        .as_ref()
                        .is_some_and(ModelBridge::can_release_idle)
                    {
                        let bridge = pane.chat.bridge.take().unwrap();
                        if !pane.chat.bridge_needs_context
                            && let Some(id) = bridge.session_id()
                            && pane.chat.shob_session_id.as_ref() != Some(&id)
                        {
                            pane.chat.shob_session_id = Some(id);
                            session_changed = true;
                        }
                        pane.chat.bridge_model = None;
                        pane.chat.bridge_resume = None;
                        pane.chat.bridge_needs_context = true;
                        Some(bridge)
                    } else {
                        None
                    };
                    let rows = if !pane.idle_resources_released {
                        pane.idle_resources_released = true;
                        pane.terminal.update(cx, |terminal, _| {
                            terminal.release_idle_resources();
                        });
                        pane.chat.view.borrow_mut().take();
                        pane.chat.focused_scrollbar.borrow_mut().take();
                        pane.screen_scratch = Vec::new();
                        Some(std::mem::take(&mut *pane.chat.row_cache.borrow_mut()))
                    } else {
                        None
                    };
                    if bridge.is_some() || rows.is_some() {
                        cx.background_executor()
                            .spawn(async move { drop((bridge, rows)) })
                            .detach();
                    }
                }
            }
        }
        session_changed
    }

    /// Persist a hibernated pane's text for crash safety. Failures are reported
    /// and ignored: the in-memory copy still wakes the pane, and losing a
    /// scrollback file is not worth failing a save over.
    fn persist_hibernated_scrollback(uid: &str, text: &str) {
        if let Err(error) = workspace_state::write_scrollback(uid, text) {
            eprintln!("failed to persist hibernated scrollback: {error}");
        }
    }

    /// Mirror a pane's conversation name onto its session, so the sidebar names
    /// the agent by what it is working on. Panes without a conversation title
    /// (plain shells) keep the OSC-derived title.
    fn sync_session_title(&mut self, pane_id: usize) {
        let Some(&(p_idx, s_idx, _)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let Some(session) = self
            .projects
            .get_mut(p_idx)
            .and_then(|project| project.sessions.get_mut(s_idx))
        else {
            return;
        };
        let primary = self
            .focused_pane_id
            .filter(|id| session.panes.iter().any(|pane| pane.id == *id))
            .or_else(|| {
                self.settings
                    .agent_layout
                    .then(|| {
                        session
                            .panes
                            .iter()
                            .find(|pane| pane.one_mode && !pane.tool_panel && pane.chat.active)
                            .map(|pane| pane.id)
                    })
                    .flatten()
            })
            .or_else(|| session.pane_tree.as_ref().map(|tree| tree.first_leaf()))
            .or_else(|| session.panes.first().map(|pane| pane.id));
        if primary != Some(pane_id) {
            return;
        }
        let Some(title) = session
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .and_then(|pane| pane.chat.conversation_title())
            .map(str::to_string)
        else {
            return;
        };
        if session.title.as_str() == title {
            return;
        }
        session.title = SharedString::from(title);
        session.display_title = truncate_title(&session.title);
    }

    fn submit_agent_prompt(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let save_history = self.settings.restore.agent_history;

        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        if pane.chat.worker_view.is_some() {
            return;
        }
        let cwd = self.agent_working_directory(pane);
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let Some(input) = pane.input_state.clone() else {
            return;
        };
        let command = input.read(cx).value().trim().to_owned();
        if command == "@browser" {
            set_input_text(&input, "@browser ".into(), window, cx);
            return;
        }
        match command.as_str() {
            "/diff" => {
                input.update(cx, |state, cx| state.set_value("", window, cx));
                self.open_project_diff(pane_id, window, cx);
                return;
            }
            "/new" => {
                input.update(cx, |state, cx| state.set_value("", window, cx));
                let project_idx = self.projects.iter().position(|project| {
                    project
                        .sessions
                        .iter()
                        .any(|session| session.panes.iter().any(|pane| pane.id == pane_id))
                });
                if let Some(project_idx) = project_idx {
                    self.create_new_session_in_project(project_idx, window, cx);
                    cx.notify();
                }
                return;
            }
            "/help" | "/version" | "/copy" => {
                let status = match command.as_str() {
                    "/help" => AGENT_SLASH_COMMANDS
                        .iter()
                        .map(|(name, description)| format!("{name} — {description}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    "/version" => format!("Shob {}", env!("CARGO_PKG_VERSION")),
                    _ => {
                        let reply = pane
                            .chat
                            .visible_messages()
                            .iter()
                            .rev()
                            .find(|message| {
                                message.role == ChatRole::Assistant && !message.text.is_empty()
                            })
                            .map(|message| message.text.clone());
                        if let Some(reply) = reply {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(reply));
                            "Copied the last assistant reply.".to_owned()
                        } else {
                            "No assistant reply to copy yet.".to_owned()
                        }
                    }
                };
                input.update(cx, |state, cx| state.set_value("", window, cx));
                pane.chat
                    .visible_messages_mut()
                    .push(Arc::new(ChatMessage::new(ChatRole::Status, status)));
                cx.notify();
                return;
            }
            _ => {}
        }
        let page = match input.read(cx).value().trim() {
            "/auth" => Some(AgentDialogPage::Auth),
            "/model" => Some(AgentDialogPage::Models),
            _ => None,
        };
        if let Some(page) = page {
            input.update(cx, |state, cx| state.set_value("", window, cx));
            self.open_agent_auth(pane_id, page, window, cx);
            return;
        }
        let mut prompt = input.read(cx).value().to_string();
        for attachment in &pane.attachments {
            if !prompt.is_empty() {
                prompt.push(' ');
            }
            prompt.push_str(&attachments::prompt_path(&attachment.path));
        }
        let trimmed = prompt.trim();
        if trimmed.is_empty() {
            return;
        }
        // Reasoning controls update the embedded session before the next prompt.
        if trimmed == "/fast" {
            input.update(cx, |state, cx| state.set_value("", window, cx));
            self.toggle_fast_mode(pane_id, cx);
            if !self.pane_ref(pane_id).is_some_and(|pane| {
                pane.chat
                    .selected_model
                    .as_ref()
                    .is_some_and(|model| model.supports_reasoning_effort())
            }) {
                return;
            }
            let fast = self
                .pane_ref(pane_id)
                .map(|p| p.chat.fast_mode)
                .unwrap_or(true);
            let msg = if fast {
                "⚡ Fast mode enabled (reduced reasoning for rapid responses)."
            } else {
                "🧠 Deep mode enabled (full reasoning for complex logic)."
            };
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.chat
                    .visible_messages_mut()
                    .push(Arc::new(ChatMessage::new(ChatRole::Status, msg.into())));
                if let Some(log) = pane.chat.visible_conversation() {
                    log.append_local(ChatRole::Status, msg);
                }
            }
            cx.notify();
            return;
        }
        if trimmed == "/thinking" || trimmed.starts_with("/thinking ") {
            let level = trimmed.trim_start_matches("/thinking").trim();
            let status = if !supported_thinking_effort(level) {
                "Usage: /thinking auto|inherit|low|medium|high|xhigh|max.".to_owned()
            } else if !pane
                .chat
                .selected_model
                .as_ref()
                .is_some_and(|model| model.supports_reasoning_effort())
            {
                "The selected model does not advertise adjustable reasoning effort.".to_owned()
            } else {
                let result = pane
                    .chat
                    .bridge
                    .as_mut()
                    .map(|bridge| bridge.set_thinking_level(level))
                    .unwrap_or(Ok(()));
                match result {
                    Ok(()) => {
                        pane.chat.thinking_override = Some(level.to_owned());
                        if let Some(model) = &pane.chat.selected_model {
                            pane.chat
                                .thinking_by_model
                                .insert(agent_model_selector(model), level.to_owned());
                        }
                        format!("Reasoning effort: {level}.")
                    }
                    Err(error) => format!("Could not set reasoning effort: {error}"),
                }
            };
            input.update(cx, |state, cx| state.set_value("", window, cx));
            pane.chat
                .messages
                .push(Arc::new(ChatMessage::new(ChatRole::Status, status)));
            self.schedule_workspace_save(cx);
            cx.notify();
            return;
        }
        if pane.chat.visible_running() {
            if pane.chat.follow_ups.len() >= 16 {
                return;
            }
            pane.chat.follow_ups.push_back(prompt);
            pane.chat.follow_ups_collapsed = false;
            pane.attachments.clear();
            input.update(cx, |state, cx| state.set_value("", window, cx));
            self.schedule_workspace_save(cx);
            cx.notify();
            return;
        }
        if Self::start_agent_turn(pane, prompt, cwd, save_history, cx) {
            pane.attachments.clear();
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
    }
}
