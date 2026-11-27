impl AppState {
    fn create_project_session(
        &mut self,
        project_idx: usize,
        agent_chat: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let self_weak = cx.entity().downgrade();
        let next_id = self.next_id;
        self.next_id += 1;

        let Some(dir) = self.projects.get(project_idx).map(|p| p.path.clone()) else {
            return;
        };
        if let Ok((terminal, pid, generation, terminal_subscription)) = Self::spawn_terminal_view(
            Some(&dir),
            self.palette.clone(),
            self_weak,
            next_id,
            None,
            self.settings.memory.scrollback(),
            cx,
        ) {
            let session_count = self
                .projects
                .get(project_idx)
                .map(|p| p.sessions.len())
                .unwrap_or(0);
            let title = SharedString::from(format!("terminal {}", session_count + 1));
            let display_title = truncate_title(&title);
            let mut pane = TerminalPane::new(
                next_id,
                generation,
                terminal.clone(),
                title.clone(),
                display_title.clone(),
                pid,
            );
            pane.chat.selected_model = self.effective_default_agent_model().cloned();
            pane.chat.permission_mode = self.settings.permission_mode;
            pane.terminal_subscription = Some(terminal_subscription);
            let (input_state, input_subscription) = build_pane_input(next_id, window, cx);
            input_state.update(cx, |state, cx| state.focus(window, cx));
            pane.input_state = Some(input_state);
            pane.input_subscription = Some(input_subscription);
            let session = Session {
                id: next_id,
                uid: paths::uid("s"),
                title: title.clone(),
                display_title: truncate_title(&title),
                logo: logos::default_logo(),
                pane_tree: Some(PaneNode::Leaf(next_id)),
                maximized_pane_id: None,
                panes: vec![pane],
                status: SessionStatus::Idle,
            };

            if let Some(project) = self.projects.get_mut(project_idx) {
                project.sessions.push(session);
                project.active_session_idx = project.sessions.len() - 1;
                project.expanded = true;
            }
            self.active_view = ActiveView::Workspace;
            self.compact_sidebar_open = false;
            self.clear_sidebar_search(window, cx);
            self.active_project_idx = project_idx;
            self.focused_pane_id = Some(next_id);
            self.rebuild_session_index();
            if agent_chat && self.settings.agent_layout {
                self.focus_one_chat(next_id, window, cx);
            }
            self.schedule_workspace_save(cx);
            cx.notify();
        }
    }

    #[cfg(test)]
    fn focus_empty_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.settings.agent_layout
            && let Some(pane) = self
                .active_session()
                .and_then(|session| self.one_mode_pane(session))
            && pane.chat.active
            && pane.chat.messages.is_empty()
            && !pane.chat.running
            && pane.attachments.is_empty()
            && pane
                .input_state
                .as_ref()
                .is_some_and(|input| input.read(cx).text().len() == 0)
        {
            let pane_id = pane.id;
            self.clear_sidebar_search(window, cx);
            self.compact_sidebar_open = false;
            self.focus_pane(pane_id, window, cx);
            return true;
        }
        false
    }

    #[cfg(test)]
    fn create_new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings.agent_layout {
            return;
        }
        if self.focus_empty_chat(window, cx) {
            return;
        }

        if self.active_project().is_some() {
            self.create_project_session(self.active_project_idx, true, window, cx);
            return;
        }

        self.select_folder_and_add_project(window, cx);
    }

    fn create_new_sidebar_session(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.projects.is_empty() {
            self.select_folder_and_add_project(window, cx);
            return;
        }
        self.sidebar_project_picker = None;
        self.sidebar_thread_menu = None;
        let project_idx = project_idx.min(self.projects.len() - 1);
        // The sidebar always opens a fresh thread, even if the current chat is empty.
        let agent_chat = self.settings.agent_layout;
        self.create_project_session(project_idx, agent_chat, window, cx);
        self.compact_sidebar_open = false;
        self.active_view = ActiveView::Workspace;
        cx.notify();
    }

    fn handle_sidebar_new_session(
        &mut self,
        local: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scoped = self.sidebar_scope_indices();
        if self.projects.is_empty() {
            self.select_folder_and_add_project(window, cx);
        } else if let Some(project_idx) = (scoped.len() == 1 && !self.settings.sidebar.project_folders)
            .then(|| scoped[0])
            .or_else(|| local.then_some(self.active_project_idx))
            .or_else(|| (self.projects.len() == 1).then_some(0))
        {
            self.choose_new_session_project(Some(project_idx), window, cx);
        } else {
            self.open_sidebar_project_picker(SidebarProjectPickerMode::NewSession, window, cx);
        }
    }

    fn sidebar_thread_state(&self, uid: &str) -> &SidebarThreadState {
        static EMPTY: std::sync::LazyLock<SidebarThreadState> =
            std::sync::LazyLock::new(SidebarThreadState::default);
        self.sidebar_threads.get(uid).unwrap_or(&EMPTY)
    }

    fn sidebar_thread_title<'a>(&'a self, session: &'a Session) -> &'a SharedString {
        self.sidebar_thread_state(&session.uid)
            .title
            .as_ref()
            .unwrap_or(&session.title)
    }

    fn sidebar_thread_uid(&self, project_idx: usize, session_idx: usize) -> Option<String> {
        self.projects
            .get(project_idx)
            .and_then(|project| project.sessions.get(session_idx))
            .map(|session| session.uid.clone())
    }

    fn sidebar_archived_thread_refs(&self) -> Vec<(usize, usize, String)> {
        let mut rows = Vec::new();
        for (project_idx, project) in self.projects.iter().enumerate() {
            for (session_idx, session) in project.sessions.iter().enumerate() {
                let state = self.sidebar_thread_state(&session.uid);
                if state.archived {
                    rows.push((project_idx, session_idx, self.sidebar_thread_title(session).to_string()));
                }
            }
        }
        rows.sort_by_key(|(project_idx, session_idx, _)| (*project_idx, *session_idx));
        rows
    }

    fn sidebar_active_thread_key(&self) -> Option<(usize, usize)> {
        let project = self.projects.get(self.active_project_idx)?;
        let session_idx = project
            .active_session_idx
            .min(project.sessions.len().saturating_sub(1));
        project.sessions.get(session_idx)?;
        Some((self.active_project_idx, session_idx))
    }

    fn sidebar_thread_id(&self, row: SidebarThreadRef) -> usize {
        self.projects
            .get(row.project_idx)
            .and_then(|project| project.sessions.get(row.session_idx))
            .map(|session| session.id)
            .unwrap_or(0)
    }

    fn sidebar_thread_is(&self, row: SidebarThreadRef, key: (usize, usize)) -> bool {
        row.project_idx == key.0 && row.session_idx == key.1
    }

    fn sidebar_session_has_title(&self, session: &Session) -> bool {
        self.sidebar_thread_state(&session.uid).title.is_some()
            || !self.settings.agent_layout
            || !self.one_mode_pane(session).is_some_and(|pane| {
                pane.chat.active
                    && (pane.chat.conversation_title().is_none()
                        || pane
                            .chat
                            .bridge
                            .as_ref()
                            .is_some_and(|bridge| bridge.waiting_for_title()))
            })
    }

    fn sidebar_thread_groups(&self) -> SidebarThreadGroups {
        let now_ms = sidebar_now_ms();
        let visible_projects: HashSet<usize> =
            self.sidebar_visible_project_indices().into_iter().collect();
        let active_key = self.sidebar_active_thread_key();
        let mut groups = SidebarThreadGroups::default();
        for (project_idx, project) in self.projects.iter().enumerate() {
            for (session_idx, session) in project.sessions.iter().enumerate() {
                let is_active = active_key == Some((project_idx, session_idx));
                if !visible_projects.contains(&project_idx) && !is_active {
                    continue;
                }
                let state = self.sidebar_thread_state(&session.uid);
                if session.status == SessionStatus::Working && !state.archived {
                    groups.working_count += 1;
                }
                if !self.sidebar_session_has_title(session) {
                    continue;
                }
                let active_section = if state.pinned {
                    SidebarSection::Pinned
                } else {
                    SidebarSection::Active
                };
                let Some(section) = state
                    .section(now_ms)
                    .or_else(|| is_active.then_some(active_section))
                else {
                    continue;
                };
                let row = SidebarThreadRef {
                    project_idx,
                    session_idx,
                    section,
                    pinned: state.pinned,
                    revealed_active: is_active,
                };
                if !self.settings.sidebar.project_folders
                    && session.status == SessionStatus::Working
                    && !state.archived
                    && !state.settled
                {
                    groups.working.push(row);
                } else {
                    match section {
                        SidebarSection::Pinned => groups.pinned.push(row),
                        SidebarSection::Active => groups.active.push(row),
                        SidebarSection::Snoozed => groups.snoozed.push(row),
                        SidebarSection::Settled => groups.settled.push(row),
                    }
                }
            }
        }
        groups
            .working
            .sort_by_key(|row| std::cmp::Reverse(self.sidebar_thread_id(*row)));
        groups.pinned.sort_by_key(|row| {
            let uid = self
                .projects[row.project_idx].sessions[row.session_idx].uid.as_str();
            (
                self.sidebar_thread_state(uid)
                    .pin_order
                    .unwrap_or(u64::MAX),
                std::cmp::Reverse(self.sidebar_thread_id(*row)),
            )
        });
        let order = self.sidebar_inbox_rank();
        groups.active.sort_by_key(|row| {
            let session = &self.projects[row.project_idx].sessions[row.session_idx];
            (order.get(session.uid.as_str()).map(|rank| rank + 1).unwrap_or(0), std::cmp::Reverse(session.id))
        });
        for shelf in [&mut groups.snoozed, &mut groups.settled] {
            shelf.sort_by_key(|row| {
                let uid = self
                    .projects[row.project_idx].sessions[row.session_idx].uid.as_str();
                (
                    std::cmp::Reverse(self.sidebar_thread_state(uid).parked_at_ms.unwrap_or(0)),
                    std::cmp::Reverse(self.sidebar_thread_id(*row)),
                )
            });
        }

        let active_limit = if self.settings.sidebar.project_folders {
            usize::MAX
        } else {
            self
            .projects
            .first()
            .map(|project| {
                project
                    .visible_session_count
                    .max(SIDEBAR_ACTIVE_INITIAL_COUNT)
            })
                .unwrap_or(SIDEBAR_ACTIVE_INITIAL_COUNT)
        };
        let active_index = active_key.and_then(|key| {
            groups
                .active
                .iter()
                .position(|row| self.sidebar_thread_is(*row, key))
        });
        let first_page = active_limit.min(groups.active.len());
        groups.hidden_active = groups.active.len() - first_page;
        let extra_active = active_index
            .filter(|index| *index >= first_page)
            .map(|index| groups.active[index]);
        groups.active.truncate(first_page);
        if let Some(row) = extra_active {
            groups.active.push(row);
            groups.hidden_active -= 1;
        }

        let settled_limit = self
            .settled_visible_count
            .max(SIDEBAR_SETTLED_INITIAL_COUNT);
        let mut visible_settled: Vec<_> =
            groups.settled.iter().take(settled_limit).copied().collect();
        if let Some(key) = active_key
            && let Some(active_row) = groups
                .settled
                .iter()
                .find(|row| self.sidebar_thread_is(**row, key))
            && !visible_settled
                .iter()
                .any(|row| self.sidebar_thread_is(*row, key))
        {
            visible_settled.push(*active_row);
        }
        groups.hidden_settled = groups.settled.len().saturating_sub(visible_settled.len());
        groups.settled = visible_settled;
        groups
    }

    fn sidebar_search_thread_refs(&self, query: &str) -> Vec<SidebarThreadRef> {
        let now_ms = sidebar_now_ms();
        let query = query.trim().to_lowercase();
        let active_key = self.sidebar_active_thread_key();
        let mut rows = Vec::new();
        for (project_idx, project) in self.projects.iter().enumerate() {
            for (session_idx, session) in project.sessions.iter().enumerate() {
                let state = self.sidebar_thread_state(&session.uid);
                if state.archived || !self.sidebar_session_has_title(session) {
                    continue;
                }
                if !sidebar_matches(&query, self.sidebar_thread_title(session).as_str())
                    && !sidebar_matches(&query, project.name.as_str())
                {
                    continue;
                }
                let section = state.section(now_ms).unwrap_or(SidebarSection::Active);
                rows.push(SidebarThreadRef {
                    project_idx,
                    session_idx,
                    section,
                    pinned: state.pinned,
                    revealed_active: active_key == Some((project_idx, session_idx)),
                });
            }
        }
        rows.sort_by_key(|row| std::cmp::Reverse(self.sidebar_thread_id(*row)));
        rows
    }

    fn sidebar_has_unsent_draft(&self, session: &Session, cx: &App) -> bool {
        session.panes.iter().any(|pane| {
            !pane.chat.agent_draft.trim().is_empty()
                || pane
                    .input_state
                    .as_ref()
                    .is_some_and(|input| !input.read(cx).value().trim().is_empty())
        })
    }

    fn sidebar_thread_status(
        &self,
        session: &Session,
        now_ms: u64,
        tokens: ThemeTokens,
    ) -> (String, Hsla) {
        let since = session
            .panes
            .iter()
            .map(|pane| pane.attention.since)
            .filter(|since| *since > 0)
            .max()
            .unwrap_or(0);
        match session.status {
            SessionStatus::Working => {
                let seconds = now_ms.saturating_sub(since) / 1000;
                let label = if since == 0 {
                    "Working".to_string()
                } else if seconds < 60 {
                    format!("Working {seconds}s")
                } else if seconds < 3_600 {
                    format!("Working {}m {}s", seconds / 60, seconds % 60)
                } else {
                    format!("Working {}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
                };
                (label, tokens.pane_border_focused)
            }
            SessionStatus::Blocked => ("Blocked".to_string(), tokens.block_failure),
            SessionStatus::Error => ("Failed".to_string(), tokens.block_failure),
            SessionStatus::Done => ("Done".to_string(), tokens.block_success),
            _ => (sidebar_relative_time(now_ms, since), tokens.sidebar_muted),
        }
    }

    fn toggle_sidebar_thread_pin(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(uid) = self.sidebar_thread_uid(project_idx, session_idx) else {
            return;
        };
        let next_order = self
            .sidebar_threads
            .values()
            .filter_map(|state| state.pin_order)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        {
            let state = self.sidebar_threads.entry(uid).or_default();
            state.pinned = !state.pinned;
            if state.pinned {
                state.pin_order = Some(next_order);
                state.settled = false;
                state.snoozed_until_ms = None;
                state.archived = false;
            } else {
                state.pin_order = None;
            }
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn move_sidebar_thread(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        delta: isize,
        cx: &mut Context<Self>,
    ) {
        let pinned = self.sidebar_thread_groups().pinned;
        let Some(position) = pinned
            .iter()
            .position(|row| row.project_idx == project_idx && row.session_idx == session_idx)
        else {
            return;
        };
        let target = position as isize + delta;
        if target < 0 || target >= pinned.len() as isize {
            return;
        }
        let neighbor = pinned[target as usize];
        let Some(uid) = self.sidebar_thread_uid(project_idx, session_idx) else {
            return;
        };
        let Some(neighbor_uid) =
            self.sidebar_thread_uid(neighbor.project_idx, neighbor.session_idx)
        else {
            return;
        };
        let current_order = self
            .sidebar_thread_state(&uid)
            .pin_order
            .unwrap_or(u64::MAX);
        let neighbor_order = self
            .sidebar_thread_state(&neighbor_uid)
            .pin_order
            .unwrap_or(u64::MAX);
        if let Some(state) = self.sidebar_threads.get_mut(&uid) {
            state.pin_order = Some(neighbor_order);
        }
        if let Some(state) = self.sidebar_threads.get_mut(&neighbor_uid) {
            state.pin_order = Some(current_order);
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn sidebar_inbox_rank(&self) -> HashMap<&str, usize> {
        self.settings.sidebar.inbox_order.iter().enumerate().map(|(index, uid)| (uid.as_str(), index)).collect()
    }

    fn drop_sidebar_threads(
        &mut self,
        drag: &SidebarThreadDrag,
        section: SidebarSection,
        target: Option<(SidebarThreadRef, bool)>,
        cx: &mut Context<Self>,
    ) {
        let ids = self.sidebar_action_selection(drag.session_id, &drag.members);
        let mut members: Vec<_> = ids.iter()
            .filter_map(|id| self.session_index.get(id))
            .map(|&(project, session)| self.projects[project].sessions[session].uid.clone())
            .collect();
        let target = target.and_then(|(row, before)| {
            self.sidebar_thread_uid(row.project_idx, row.session_idx).map(|uid| (uid, before))
        });
        if members.is_empty() || target.as_ref().is_some_and(|(uid, _)| members.contains(uid)) {
            return;
        }
        let now = sidebar_now_ms();
        for uid in &members {
            let state = self.sidebar_threads.entry(uid.clone()).or_default();
            state.pinned = section == SidebarSection::Pinned;
            state.settled = section == SidebarSection::Settled;
            state.archived = false;
            state.pin_order = None;
            state.snoozed_until_ms = (section == SidebarSection::Snoozed)
                .then(|| now.saturating_add(SIDEBAR_SNOOZE_MS));
            state.parked_at_ms = (state.settled || state.snoozed_until_ms.is_some()).then_some(now);
        }
        if section == SidebarSection::Pinned {
            let moved: HashSet<&str> = members.iter().map(String::as_str).collect();
            let mut pinned: Vec<_> = self.sidebar_threads.iter()
                .filter(|(uid, state)| state.pinned && !moved.contains(uid.as_str()))
                .map(|(uid, state)| (state.pin_order.unwrap_or(u64::MAX), uid.clone()))
                .collect();
            pinned.sort_unstable();
            let index = target.as_ref().and_then(|(target, before)| {
                pinned.iter().position(|(_, uid)| uid == target).map(|index| index + usize::from(!before))
            }).unwrap_or(pinned.len());
            pinned.splice(index..index, members.drain(..).map(|uid| (0, uid)));
            for (index, (_, uid)) in pinned.into_iter().enumerate() {
                if let Some(state) = self.sidebar_threads.get_mut(&uid) {
                    state.pin_order = Some(index as u64);
                }
            }
        } else if section == SidebarSection::Settled {
            self.settings.sidebar.settled_expanded = true;
        } else if section == SidebarSection::Active && let Some((target, before)) = target {
            let rank = self.sidebar_inbox_rank();
            let moved: HashSet<&str> = members.iter().map(String::as_str).collect();
            let mut inbox: Vec<_> = self.projects.iter().flat_map(|project| &project.sessions)
                .filter(|session| !moved.contains(session.uid.as_str()))
                .collect();
            inbox.sort_by_key(|session| (
                rank.get(session.uid.as_str()).map(|rank| rank + 1).unwrap_or(0),
                std::cmp::Reverse(session.id)
            ));
            let Some(position) = inbox.iter().position(|session| session.uid == target) else { return };
            let mut order: Vec<_> = inbox.into_iter().map(|session| session.uid.clone()).collect();
            let index = position + usize::from(!before);
            order.splice(index..index, members);
            self.settings.sidebar.inbox_order = order;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn settle_sidebar_thread(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(uid) = self.sidebar_thread_uid(project_idx, session_idx) else {
            return;
        };
        {
            let state = self.sidebar_threads.entry(uid).or_default();
            state.settled = true;
            state.pinned = false;
            state.pin_order = None;
            state.snoozed_until_ms = None;
            state.archived = false;
            state.parked_at_ms = Some(sidebar_now_ms());
        }
        self.settings.sidebar.settled_expanded = true;
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn unsettle_sidebar_thread(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(uid) = self.sidebar_thread_uid(project_idx, session_idx) else {
            return;
        };
        {
            let state = self.sidebar_threads.entry(uid).or_default();
            state.settled = false;
            state.snoozed_until_ms = None;
            state.parked_at_ms = None;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn snooze_sidebar_thread(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        duration_ms: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(uid) = self.sidebar_thread_uid(project_idx, session_idx) else {
            return;
        };
        {
            let now = sidebar_now_ms();
            let state = self.sidebar_threads.entry(uid).or_default();
            state.snoozed_until_ms = Some(now.saturating_add(duration_ms));
            state.settled = false;
            state.pinned = false;
            state.pin_order = None;
            state.parked_at_ms = Some(now);
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn wake_sidebar_thread(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(uid) = self.sidebar_thread_uid(project_idx, session_idx) else {
            return;
        };
        {
            let state = self.sidebar_threads.entry(uid).or_default();
            state.snoozed_until_ms = None;
            state.parked_at_ms = None;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn archive_sidebar_thread(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(uid) = self.sidebar_thread_uid(project_idx, session_idx) else {
            return;
        };
        {
            let state = self.sidebar_threads.entry(uid).or_default();
            state.archived = true;
            state.pinned = false;
            state.pin_order = None;
            state.settled = false;
            state.snoozed_until_ms = None;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn restore_sidebar_thread(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(uid) = self.sidebar_thread_uid(project_idx, session_idx) else {
            return;
        };
        {
            let state = self.sidebar_threads.entry(uid).or_default();
            state.archived = false;
            state.settled = false;
            state.snoozed_until_ms = None;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn set_sidebar_active_visible_count(&mut self, count: usize, cx: &mut Context<Self>) {
        for project in &mut self.projects {
            project.visible_session_count = count;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn show_more_settled_threads(&mut self, cx: &mut Context<Self>) {
        self.settled_visible_count = self
            .settled_visible_count
            .saturating_add(SIDEBAR_SETTLED_PAGE_COUNT);
        cx.notify();
    }

    /// Close one session (all of its panes) without touching the other
    /// sessions and projects. The project stays open, with an empty state that
    /// offers to create a new session.
    fn close_session(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get(project_idx) else {
            return;
        };
        if session_idx >= project.sessions.len() {
            return;
        }
        let session = &project.sessions[session_idx];
        let live_pane_uids: HashSet<&str> = self
            .projects
            .iter()
            .enumerate()
            .flat_map(|(p, project)| {
                project
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(move |(s, _)| p != project_idx || *s != session_idx)
                    .flat_map(|(_, session)| session.panes.iter().map(|pane| pane.uid.as_str()))
            })
            .collect();
        let deletion = workspace_state::PendingDeletion {
            session_uid: session.uid.clone(),
            pane_uids: session
                .panes
                .iter()
                .filter(|pane| !live_pane_uids.contains(pane.uid.as_str()))
                .map(|pane| pane.uid.clone())
                .collect(),
            shob_session_ids: session
                .panes
                .iter()
                .filter(|pane| !live_pane_uids.contains(pane.uid.as_str()))
                .filter_map(|pane| pane.chat.shob_session_id.clone())
                .filter(|id| {
                    !self
                        .projects
                        .iter()
                        .enumerate()
                        .flat_map(|(p, project)| {
                            project
                                .sessions
                                .iter()
                                .enumerate()
                                .filter(move |(s, _)| p != project_idx || *s != session_idx)
                                .flat_map(|(_, session)| session.panes.iter())
                        })
                        .any(|pane| pane.chat.shob_session_id.as_ref() == Some(id))
                })
                .collect(),
        };
        let deletion_path = match workspace_state::record_deletion(&deletion) {
            Ok(path) => path,
            Err(error) => {
                window.push_notification(Notification::new().title("Could not delete session")
                    .message(format!("Could not record deletion: {error}. Check the data folder permissions and try again."))
                    .with_type(NotificationType::Error), cx);
                return;
            }
        };
        let project = &mut self.projects[project_idx];
        let removed = project.sessions.remove(session_idx);
        let removed_uid = removed.uid.clone();
        for pane in &removed.panes {
            status::forget_pid(pane.pid);
        }
        if project.active_session_idx > session_idx {
            project.active_session_idx -= 1;
        }
        if project.active_session_idx >= project.sessions.len() {
            project.active_session_idx = project.sessions.len().saturating_sub(1);
        }
        let is_active_project = project_idx == self.active_project_idx;
        let pane_id = project
            .sessions
            .get(project.active_session_idx)
            .and_then(|s| s.panes.first())
            .map(|pane| pane.id);
        self.rebuild_session_index();
        if is_active_project {
            if let Some(pane_id) = pane_id {
                self.focus_workspace_pane(pane_id, window, cx);
            } else {
                self.focused_pane_id = None;
            }
        }
        // Drop the removed terminals after forgetting their pids, so their
        // PTY masters close and the shells go away with them.
        drop(removed);
        self.sidebar_threads.remove(&removed_uid);
        self.settings.sidebar.inbox_order.retain(|uid| uid != &removed_uid);
        // Invalidate older debounced snapshots before publishing the deletion.
        self.save_generation.fetch_add(1, Ordering::SeqCst);
        let saved = if self.persistence_enabled {
            match self.persistence_path.as_ref() {
                Some(path) => workspace_state::save_atomic(path, &self.saved_workspace(cx)),
                None => Err(std::io::Error::other("workspace path unavailable")),
            }
        } else {
            Ok(())
        };
        let result =
            saved.and_then(|()| workspace_state::finish_deletion(&deletion_path, &deletion));
        if let Err(error) = result {
            window.push_notification(Notification::new().title("Session deletion incomplete")
                .message(format!("{error}. Check the data folder permissions; cleanup will retry when Shob restarts."))
                .with_type(NotificationType::Error), cx);
        }
        cx.notify();
        self.schedule_workspace_save(cx);
    }

    /// Close a whole project. Its sessions and PTYs go away; the remaining
    /// projects (and their sessions) are left exactly as they were.
    fn close_project(&mut self, project_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if project_idx >= self.projects.len() {
            return;
        }
        let project_id = self.projects[project_idx].id;
        if self.project_menu == Some(project_id) {
            self.project_menu = None;
        }
        if self.renaming_project.as_ref().map(|r| r.project_id) == Some(project_id) {
            self.renaming_project = None;
        }
        let was_active = project_idx == self.active_project_idx;
        let removed = self.projects.remove(project_idx);
        let removed_path = removed.path.clone();
        let removed_uids: HashSet<String> = removed
            .sessions
            .iter()
            .map(|session| session.uid.clone())
            .collect();
        for session in &removed.sessions {
            for pane in &session.panes {
                status::forget_pid(pane.pid);
            }
        }
        if self.active_project_idx > project_idx {
            self.active_project_idx -= 1;
        }
        if self.active_project_idx >= self.projects.len() {
            self.active_project_idx = self.projects.len().saturating_sub(1);
        }
        drop(removed);
        self.sidebar_threads
            .retain(|uid, _| !removed_uids.contains(uid));
        let sidebar = &mut self.settings.sidebar;
        sidebar.pinned_projects.retain(|path| path != &removed_path);
        sidebar.project_order.retain(|path| path != &removed_path);
        self.sidebar_folder_show_all.remove(&project_id);
        let count = sidebar.extra_project_scopes.len();
        sidebar.extra_project_scopes.retain(|path| path != &removed_path);
        if sidebar.project_scope.as_ref() == Some(&removed_path) {
            sidebar.project_scope = sidebar.extra_project_scopes.pop();
        }
        if count != sidebar.extra_project_scopes.len() || sidebar.project_scope.is_none() {
            self.settled_visible_count = SIDEBAR_SETTLED_INITIAL_COUNT;
        }
        self.rebuild_session_index();
        if was_active && !self.projects.is_empty() {
            let next_idx = self.active_project_idx;
            self.activate_project(next_idx, window, cx);
        } else if self.projects.is_empty() {
            self.focused_pane_id = None;
        }
        cx.notify();
        self.schedule_workspace_save(cx);
    }

    fn start_renaming_project(
        &mut self,
        project_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_menu = None;
        if let Some(project) = self.projects.iter().find(|p| p.id == project_id) {
            let current_name = project.name.clone();
            let input = cx.new(|cx| InputState::new(window, cx).default_value(current_name));
            input.update(cx, |state, cx| {
                state.focus(window, cx);
                state.select_all_text(cx);
            });
            let subscription = cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let new_name = input.read(cx).value();
                    this.finish_renaming_project(project_id, new_name, cx);
                }
            });
            self.renaming_project = Some(RenamingProject {
                project_id,
                input,
                _subscription: subscription,
            });
        }
        cx.notify();
    }
}
