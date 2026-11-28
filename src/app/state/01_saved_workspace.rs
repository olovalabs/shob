impl AppState {
    fn saved_workspace(&self, cx: &App) -> workspace_state::SavedWorkspace {
        let live_session_uids: HashSet<&str> = self
            .projects
            .iter()
            .flat_map(|project| project.sessions.iter().map(|session| session.uid.as_str()))
            .collect();
        let mut threads: Vec<_> = self
            .sidebar_threads
            .iter()
            .filter(|(uid, state)| {
                live_session_uids.contains(uid.as_str()) && **state != SidebarThreadState::default()
            })
            .map(|(uid, state)| workspace_state::SavedSidebarThread {
                session_uid: uid.clone(),
                pinned: state.pinned,
                pin_order: state.pin_order,
                settled: state.settled,
                snoozed_until_ms: state.snoozed_until_ms,
                archived: state.archived,
                parked_at_ms: state.parked_at_ms,
                title: state.title.as_ref().map(ToString::to_string),
            })
            .collect();
        threads.sort_by(|a, b| a.session_uid.cmp(&b.session_uid));
        let projects = self
            .projects
            .iter()
            .map(|project| {
                let sessions = project
                    .sessions
                    .iter()
                    .map(|session| {
                        let panes = session
                            .panes
                            .iter()
                            .map(|pane| {
                                let live = pane
                                    .input_state
                                    .as_ref()
                                    .map(|input| input.read(cx).value().to_string())
                                    .or_else(|| pane.restore_draft.clone())
                                    .unwrap_or_default();
                                let (draft, agent_draft) = if pane.chat.active {
                                    (pane.chat.terminal_draft.clone(), live)
                                } else {
                                    (live, pane.chat.agent_draft.clone())
                                };
                                workspace_state::SavedPane {
                                    id: pane.id,
                                    uid: pane.uid.clone(),
                                    title: pane.title.clone(),
                                    display_title: pane.display_title.to_string(),
                                    cwd: pane.cwd.clone().or_else(|| self.pane_cwd_path(pane)),
                                    draft,
                                    agent_draft,
                                    queued_messages: pane.chat.follow_ups.iter().cloned().collect(),
                                    command_history: pane
                                        .command_history
                                        .iter()
                                        .rev()
                                        .take(200)
                                        .rev()
                                        .cloned()
                                        .collect(),
                                    chat_active: pane.chat.active || pane.legacy_chat,
                                    one_mode: pane.one_mode,
                                    regular_focus: pane.regular_focus,
                                    tool_panel_owner: pane
                                        .tool_panel
                                        .then_some(pane.preview_owner)
                                        .flatten(),
                                    tool_panel_changes: pane.tool_panel
                                        && pane
                                            .diff_preview
                                            .as_ref()
                                            .is_some_and(|view| !view.file_only),
                                    tool_panel_tab: pane.tool_panel_tab,
                                    tool_panel_hidden: pane.tool_panel_hidden,
                                    tool_panel_chooser_open: pane.tool_panel_chooser.is_some(),
                                    tool_panel_file: if pane.tool_panel {
                                        pane.markdown_preview
                                            .as_ref()
                                            .map(|(path, _)| path.clone())
                                            .or_else(|| {
                                                let view = pane
                                                    .diff_preview
                                                    .as_ref()
                                                    .filter(|view| view.file_only)?;
                                                Some(
                                                    view.directory
                                                        .join(&view.file_view.as_ref()?.path),
                                                )
                                            })
                                            .or_else(|| pane.tool_panel_file_restore.clone())
                                    } else {
                                        None
                                    },
                                    tool_panel_chat_ratio: pane.tool_panel_chat_ratio,
                                    files: pane
                                        .files_surface
                                        .as_ref()
                                        .map(|(view, _)| {
                                            let mut saved = view.read(cx).snapshot(cx);
                                            saved.active = pane.files_active;
                                            saved
                                        })
                                        .or_else(|| pane.files_restore.clone()),
                                    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
                                    browser: pane.browser.as_ref().map(|browser| match browser {
                                        ToolPanelBrowser::Saved(saved) => saved.clone(),
                                        ToolPanelBrowser::Mounted(browser) => {
                                            browser.view.read(cx).snapshot()
                                        }
                                    }),
                                    #[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
                                    browser: None,
                                    chat: workspace_state::SavedChat {
                                        fast_mode: pane.chat.fast_mode,
                                        thinking_override: pane.chat.thinking_override.clone(),
                                        thinking_by_model: pane.chat.thinking_by_model.clone(),
                                        selected_model: pane.chat.selected_model.clone(),
                                        first_response_provider: pane
                                            .chat
                                            .first_response_provider
                                            .clone(),
                                        model_explicit: pane.chat.model_explicit,
                                        agent_title: pane.chat.agent_title.clone(),
                                        prompt_title: pane.chat.prompt_title.clone(),
                                        shob_session_id: pane.chat.shob_session_id.clone(),
                                        conversation_key: (self.settings.restore.agent_history
                                            || pane.chat.conversation_key != "default")
                                            .then(|| pane.chat.conversation_key.clone()),
                                        workspace_override: pane.chat.workspace_override.clone(),
                                        models: pane.chat.legacy_models.clone(),
                                    },
                                }
                            })
                            .collect();
                        workspace_state::SavedSession {
                            id: session.id,
                            uid: session.uid.clone(),
                            title: session.title.to_string(),
                            display_title: session.display_title.to_string(),
                            logo: session.logo.to_string(),
                            maximized_pane_id: session.maximized_pane_id,
                            focused_pane: self
                                .focused_pane_id
                                .filter(|id| session.panes.iter().any(|pane| pane.id == *id))
                                .or_else(|| session.panes.first().map(|pane| pane.id)),
                            tree: session
                                .pane_tree
                                .as_ref()
                                .map(save_pane_tree)
                                .unwrap_or_else(|| workspace_state::SavedPaneNode::Leaf {
                                    pane: session.panes.first().map(|pane| pane.id).unwrap_or(0),
                                }),
                            panes,
                        }
                    })
                    .collect();
                workspace_state::SavedProject {
                    id: project.id,
                    path: project.path.clone(),
                    name: project.name.to_string(),
                    projectless: project.projectless,
                    expanded: project.expanded,
                    active_session: project.active_session_idx,
                    sessions,
                }
            })
            .collect();
        workspace_state::SavedWorkspace {
            version: workspace_state::CURRENT_VERSION,
            active_project: self.active_project().map(|project| project.path.clone()),
            projects,
            threads,
        }
    }

    fn saved_settings(&self) -> settings::AppSettings {
        let mut saved = self.settings.clone();
        saved.sidebar.visible = self.sidebar_visible;
        saved.sidebar.width = f32::from(self.sidebar_width);
        saved.theme = self.theme_preference;
        saved.palette_scheme = self.palette_scheme;
        saved.default_agent_model = self.default_agent_model.clone();
        if let Some(window) = self.last_window_bounds {
            saved.window = Some(window);
        }
        saved
    }

    fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.schedule_workspace_save(cx);
    }

    fn schedule_workspace_save(&mut self, cx: &Context<Self>) {
        if !self.startup_loaded {
            return;
        }
        self.dirty = true;
        if !self.persistence_enabled {
            return;
        }
        let Some(path) = self.persistence_path.clone() else {
            return;
        };
        let generation = self.save_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let generation_counter = self.save_generation.clone();
        self.workspace_save_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            executor.timer(std::time::Duration::from_millis(300)).await;
            let Ok((workspace, settings)) = this.update(cx, |state, cx| {
                (
                    (!state.workspace_cleared).then(|| state.saved_workspace(cx)),
                    state.saved_settings(),
                )
            }) else {
                return;
            };
            executor
                .spawn(async move {
                    if let Some(workspace) = workspace {
                        match workspace_state::save_if_current(
                            &path,
                            &workspace,
                            &generation_counter,
                            generation,
                        ) {
                            Ok(true) => {}
                            Ok(false) => return,
                            Err(error) => eprintln!("failed to save workspace state: {error}"),
                        }
                    }
                    if let Err(error) = settings::save_preserving_theme_if_current(
                        &settings,
                        &generation_counter,
                        generation,
                    ) {
                        eprintln!("failed to save settings: {error}");
                    }
                })
                .await;
        }));
    }

    fn capture_window_bounds(&mut self, cx: &mut App) {
        let handles = cx.windows();
        for handle in handles {
            if let Ok(bounds) = handle.update(cx, |_root, window, _cx| window.window_bounds()) {
                self.last_window_bounds = Some(settings::SavedWindow::from_bounds(bounds));
                break;
            }
        }
    }

    fn pane_scrollback(&self, pane: &TerminalPane, cx: &App) -> Option<String> {
        if pane.is_browser() {
            return None;
        }
        let terminal = pane.terminal.read(cx);
        if terminal.is_alt_screen() {
            return None;
        }
        let text = terminal.buffer_text(workspace_state::max_scrollback_lines());
        (!text.trim().is_empty()).then_some(text)
    }

    /// Synchronous, best-effort flush used when the window closes or the app
    /// quits. Writes settings, workspace, scrollback and conversation logs so a
    /// normal close never loses the last few seconds of work.
    fn save_all_sync(&mut self, window: Option<&Window>, cx: &mut App) {
        if !self.startup_loaded {
            return;
        }
        if !self.persistence_enabled {
            self.dirty = false;
            return;
        }
        self.save_generation.fetch_add(1, Ordering::SeqCst);
        self.workspace_save_task = None;
        if let Some(window) = window {
            self.last_window_bounds =
                Some(settings::SavedWindow::from_bounds(window.window_bounds()));
        } else {
            self.capture_window_bounds(cx);
        }

        if !self.workspace_cleared
            && let Some(path) = &self.persistence_path
            && let Err(error) = workspace_state::save_atomic(path, &self.saved_workspace(cx))
        {
            eprintln!("failed to save workspace state: {error}");
        }
        if let Err(error) = settings::save_preserving_theme(&self.saved_settings()) {
            eprintln!("failed to save settings: {error}");
        }

        let generation = self.scrollback_generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.autosave_task = None;
        if self.saves_scrollback() {
            let mut written = HashSet::new();
            for pane in self.projects.iter().flat_map(|project| &project.sessions).flat_map(|session| &session.panes) {
                if written.insert(pane.uid.as_str())
                    && let Some(text) = self.pane_scrollback(pane, cx)
                {
                    Self::write_pane_scrollback(&pane.uid, &text, &self.scrollback_generation, generation);
                }
            }
        }
        conversations::flush_pending();
        self.dirty = false;
    }

    fn saves_scrollback(&self) -> bool {
        self.persistence_enabled && !self.workspace_cleared && self.settings.restore.scrollback
    }

    fn write_pane_scrollback(uid: &str, text: &str, counter: &AtomicU64, generation: u64) {
        if let Err(error) = workspace_state::write_scrollback_if_current(uid, text, counter, generation) {
            eprintln!("failed to save scrollback: {error}");
        }
    }

    fn autosave(&mut self, cx: &mut Context<Self>) {
        if !self.dirty || !self.persistence_enabled || self.autosave_task.is_some() {
            return;
        }
        self.capture_window_bounds(cx);
        self.schedule_workspace_save(cx);
        self.dirty = false;
        let generation = self.scrollback_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let counter = self.scrollback_generation.clone();
        let mut panes = Vec::new();
        if self.saves_scrollback() {
            let mut seen = HashSet::new();
            panes.extend(self.projects.iter().flat_map(|project| &project.sessions)
                .flat_map(|session| &session.panes)
                .filter(|pane| seen.insert(pane.uid.as_str()))
                .map(|pane| pane.id));
        }
        self.autosave_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            for id in panes {
                if counter.load(Ordering::SeqCst) != generation {
                    break;
                }
                let snapshot = this.update(cx, |this, cx| {
                    let pane = this.pane_ref(id)?;
                    Some((pane.uid.clone(), this.pane_scrollback(pane, cx)?))
                });
                let Ok(snapshot) = snapshot else { return };
                if let Some((uid, text)) = snapshot {
                    let counter = counter.clone();
                    executor.spawn(async move {
                        Self::write_pane_scrollback(&uid, &text, &counter, generation);
                    }).await;
                }
            }
            executor.spawn(async { conversations::flush_pending() }).await;
            let _ = this.update(cx, |this, _| this.autosave_task = None);
        }));
    }

    fn apply_saved_workspace(&mut self, restored: RestoreData, cx: &mut Context<Self>) {
        if self.startup_loaded {
            return;
        }
        self.startup_loaded = true;

        self.sidebar_visible = self.settings.sidebar.visible;
        self.sidebar_width = px(self.settings.sidebar.width).max(SIDEBAR_MIN_WIDTH);
        self.theme_preference = self.settings.theme;
        self.palette_scheme = self.settings.palette_scheme;
        self.default_agent_model = self.settings.default_agent_model.clone();
        let mode = self.theme_preference.resolve_mode(cx.window_appearance());
        self.palette = self
            .settings
            .theme_colors
            .palette(self.palette_scheme, mode.is_dark());
        theme::apply_component_theme(mode, None, cx);
        sync_component_fonts(cx);
        theme::sync_imported_components(self.palette_scheme, self.tokens(cx), cx);

        let RestoreData {
            workspace,
            mut scrollbacks,
            transcripts,
            usage,
        } = restored;
        self.next_id = self.next_id.max(workspace.max_id().saturating_add(1));
        let active_project_path = workspace.active_project.clone();
        let saved_threads = workspace.threads.clone();
        let mut focus_by_session: HashMap<usize, Option<usize>> = HashMap::new();

        self.sidebar_threads = saved_threads
            .into_iter()
            .map(|saved| {
                (
                    saved.session_uid,
                    SidebarThreadState {
                        pinned: saved.pinned,
                        pin_order: saved.pin_order,
                        settled: saved.settled,
                        snoozed_until_ms: saved.snoozed_until_ms,
                        archived: saved.archived,
                        parked_at_ms: saved.parked_at_ms,
                        title: saved.title.map(SharedString::from),
                    },
                )
            })
            .collect();
        self.settled_visible_count = SIDEBAR_SETTLED_INITIAL_COUNT;
        self.projects.clear();
        self.active_project_idx = 0;

        for project in workspace.projects {
            if !project.path.is_dir() || self.projects.iter().any(|p| p.path == project.path) {
                continue;
            }
            let project_path = project.path.clone();
            let mut sessions = Vec::new();

            for saved_session in project.sessions {
                let scratch_dir = project
                    .projectless
                    .then(|| project_path.join(&saved_session.uid))
                    .filter(|dir| dir.is_dir());
                let session_dir = scratch_dir.as_ref().unwrap_or(&project_path);
                let mut panes = Vec::new();
                for saved_pane in saved_session.panes {
                    let dir = saved_pane
                        .cwd
                        .clone()
                        .filter(|cwd| cwd.is_dir())
                        .unwrap_or_else(|| session_dir.clone());
                    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
                    if let Some(owner) = saved_pane.tool_panel_owner
                        && let Some(browser) = saved_pane.browser
                    {
                        let mut pane = Self::new_browser_pane(saved_pane.id, owner, dir, browser, cx);
                        if !saved_pane.uid.is_empty() {
                            pane.uid = saved_pane.uid;
                        }
                        panes.push(pane);
                        continue;
                    }
                    let scrollback = if self.settings.restore.scrollback {
                        scrollbacks.remove(&saved_pane.uid)
                    } else {
                        None
                    };
                    let weak = cx.entity().downgrade();
                    let Ok((terminal, pid, generation, terminal_subscription)) =
                        Self::spawn_terminal_view(
                            Some(&dir),
                            self.palette.clone(),
                            weak,
                            saved_pane.id.max(1),
                            scrollback,
                            self.settings.memory.scrollback(),
                            cx,
                        )
                    else {
                        continue;
                    };

                    let title = SharedString::from(if saved_pane.browser.is_some()
                        || saved_pane.title.is_empty()
                    {
                        "terminal".to_string()
                    } else {
                        saved_pane.title.clone()
                    });
                    let display_title =
                        SharedString::from(if saved_pane.browser.is_some() {
                            "Terminal".to_string()
                        } else if saved_pane.display_title.is_empty() {
                            title.to_string()
                        } else {
                            saved_pane.display_title.clone()
                        });
                    let mut pane = TerminalPane::new(
                        saved_pane.id,
                        generation,
                        terminal,
                        title,
                        display_title,
                        pid,
                    );
                    pane.uid = if saved_pane.uid.is_empty() {
                        paths::uid("p")
                    } else {
                        saved_pane.uid.clone()
                    };
                    pane.cwd = Some(dir);
                    pane.one_mode = saved_pane.one_mode;
                    pane.regular_focus = saved_pane.regular_focus;
                    pane.preview_owner = saved_pane.tool_panel_owner;
                    pane.tool_panel = saved_pane.tool_panel_owner.is_some();
                    pane.tool_panel_tab = saved_pane.tool_panel_tab;
                    pane.tool_panel_hidden = saved_pane.tool_panel_hidden;
                    pane.tool_panel_chooser = saved_pane
                        .tool_panel_chooser_open
                        .then(|| cx.focus_handle());
                    pane.tool_panel_file_restore = saved_pane.tool_panel_file;
                    pane.tool_panel_chat_ratio = saved_pane
                        .tool_panel_chat_ratio
                        .filter(|ratio| ratio.is_finite())
                        .map(|ratio| ratio.clamp(0.0, 1.0));
                    if pane.tool_panel && saved_pane.tool_panel_changes {
                        pane.diff_preview = Some(DiffPreview {
                            directory: session_dir.clone(),
                            result: None,
                            expanded: HashSet::new(),
                            tracked_collapsed: false,
                            untracked_collapsed: false,
                            request: 0,
                            file_request: 0,
                            file_view: None,
                            file_only: false,
                        });
                    }
                    if let Some(saved) = saved_pane.files {
                        pane.files_active = saved.active;
                        pane.files_restore = Some(saved);
                    }
                    pane.command_history = saved_pane.command_history;
                    pane.restore_draft = Some(saved_pane.draft.clone());
                    pane.legacy_chat = saved_pane.chat_active && !pane.one_mode && !pane.tool_panel;
                    pane.chat.active = saved_pane.chat_active && pane.one_mode && !pane.tool_panel;
                    if pane.chat.active {
                        pane.restore_draft = Some(saved_pane.agent_draft.clone());
                    }
                    pane.chat.fast_mode = saved_pane.chat.fast_mode;
                    pane.chat.thinking_override = saved_pane
                        .chat
                        .thinking_override
                        .clone()
                        .filter(|level| supported_thinking_effort(level));
                    pane.chat.thinking_by_model = saved_pane.chat.thinking_by_model.clone();
                    // The app-wide selection is authoritative for every pane.
                    pane.chat.model_explicit = false;
                    pane.chat.agent_title = saved_pane.chat.agent_title.clone();
                    pane.chat.prompt_title = saved_pane.chat.prompt_title.clone();
                    pane.chat.selected_model = self.effective_default_agent_model().cloned();
                    pane.chat.permission_mode = self.settings.permission_mode;
                    pane.chat.first_response_provider =
                        saved_pane.chat.first_response_provider.clone();
                    pane.chat.shob_session_id = saved_pane.chat.shob_session_id.clone();
                    pane.chat.workspace_override = saved_pane
                        .chat
                        .workspace_override
                        .clone()
                        .filter(|path| path.is_dir());
                    if let Some(override_dir) = pane.chat.workspace_override.as_ref() {
                        pane.chat.cwd = Some(override_dir.clone());
                    }
                    pane.chat.terminal_draft = saved_pane.draft.clone();
                    pane.chat.agent_draft = saved_pane.agent_draft.clone();
                    pane.chat.follow_ups = saved_pane
                        .queued_messages
                        .iter()
                        .take(16)
                        .cloned()
                        .collect();
                    pane.chat.follow_ups_paused = !pane.chat.follow_ups.is_empty();

                    let saved = &saved_pane.chat;
                    pane.chat.legacy_models = saved.models.clone();
                    if let Some(key) = saved.valid_conversation_key() {
                        pane.chat.conversation_key = key.to_owned();
                    } else if self.settings.restore.agent_history
                        && let Some(legacy) = saved.models.iter().find(|legacy| {
                            legacy.model.is_some() && legacy.model == saved.selected_model
                        })
                    {
                        let key = conversations::legacy_conversation_key(legacy.model.as_ref());
                        if legacy.shob_session_id.is_some()
                            || transcripts.contains_key(&transcript_key(&pane.uid, &key))
                        {
                            pane.chat.conversation_key = key;
                            pane.chat.shob_session_id = legacy.shob_session_id.clone();
                            pane.chat.agent_title = legacy.agent_title.clone();
                            pane.chat.prompt_title = legacy.prompt_title.clone();
                        }
                    }
                    if self.settings.restore.agent_history {
                        let key = &pane.chat.conversation_key;
                        pane.chat.conversation = Some(conversations::Log::open(&pane.uid, key));
                        if let Some(restored) = transcripts.get(&transcript_key(&pane.uid, key)) {
                            pane.chat.messages = restored.messages.clone();
                            pane.chat.checklist.list = restored.checklist.clone();
                        }
                        if let Some(total) = usage.get(&transcript_key(&pane.uid, key)) {
                            pane.chat.set_usage(*total);
                        }
                    }
                    if !pane.one_mode && !pane.tool_panel {
                        pane.legacy_chat |= !pane.chat.agent_draft.is_empty()
                            || !pane.chat.messages.is_empty()
                            || pane.chat.shob_session_id.is_some();
                    }

                    pane.terminal_subscription = Some(terminal_subscription);
                    panes.push(pane);
                }

                if panes.is_empty() {
                    continue;
                }
                for owner_index in 0..panes.len() {
                    let owner = &panes[owner_index];
                    let Some(tab) = owner.tool_panel_tab else {
                        continue;
                    };
                    if let Some(position) = panes
                        .iter()
                        .filter(|pane| pane.tool_panel && pane.preview_owner == Some(owner.id))
                        .position(|pane| pane.id == tab)
                    {
                        let scroll = gpui::ScrollHandle::new();
                        scroll.scroll_to_item(position);
                        panes[owner_index].tool_panel_scroll = Some(scroll);
                    }
                }
                let tree = restore_pane_tree(&saved_session.tree);
                let tree = if tree_matches_panes(&tree, &panes)
                    && panes.iter().all(|pane| tree.contains(pane.id))
                {
                    tree
                } else {
                    PaneNode::Leaf(panes[0].id)
                };
                let maximized = saved_session
                    .maximized_pane_id
                    .filter(|id| panes.iter().any(|pane| pane.id == *id));
                let title = SharedString::from(if saved_session.title.is_empty() {
                    "terminal".to_string()
                } else {
                    saved_session.title.clone()
                });
                let display_title = SharedString::from(if saved_session.display_title.is_empty() {
                    title.to_string()
                } else {
                    saved_session.display_title.clone()
                });
                let session_id = saved_session.id.max(1);
                focus_by_session.insert(session_id, saved_session.focused_pane);
                sessions.push(Session {
                    id: session_id,
                    uid: if saved_session.uid.is_empty() {
                        paths::uid("s")
                    } else {
                        saved_session.uid.clone()
                    },
                    scratch_dir,
                    title,
                    display_title,
                    logo: if saved_session.logo.is_empty() {
                        logos::default_logo()
                    } else {
                        SharedString::from(saved_session.logo)
                    },
                    pane_tree: Some(tree),
                    maximized_pane_id: maximized,
                    panes,
                    status: SessionStatus::Idle,
                });
            }

            let active_session_idx = project.active_session.min(sessions.len().saturating_sub(1));
            self.projects.push(Project {
                id: if project.id == 0 {
                    let id = self.next_id;
                    self.next_id += 1;
                    id
                } else {
                    project.id
                },
                name: if project.projectless {
                    NO_PROJECT_LABEL.into()
                } else {
                    project.name.into()
                },
                path: project.path,
                projectless: project.projectless,
                sessions,
                active_session_idx,
                expanded: project.expanded,
                visible_session_count: 5,
            });
        }

        self.active_project_idx = active_project_path
            .as_ref()
            .and_then(|path| {
                self.projects
                    .iter()
                    .position(|project| &project.path == path)
            })
            .unwrap_or(0);

        if self.projects.is_empty() {
            let default_dir = std::env::current_dir()
                .ok()
                .or_else(|| directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from("/"));
            let name = default_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            let id = self.next_id;
            self.next_id += 1;
            let weak = cx.entity().downgrade();
            if let Ok((terminal, pid, generation, terminal_subscription)) =
                Self::spawn_terminal_view(
                    Some(&default_dir),
                    self.palette.clone(),
                    weak,
                    id,
                    None,
                    self.settings.memory.scrollback(),
                    cx,
                )
            {
                let title = SharedString::from("terminal 1");
                let mut pane =
                    TerminalPane::new(id, generation, terminal, title.clone(), title.clone(), pid);
                pane.cwd = Some(default_dir.clone());
                pane.chat.selected_model = self.effective_default_agent_model().cloned();
                pane.chat.permission_mode = self.settings.permission_mode;
                pane.terminal_subscription = Some(terminal_subscription);
                let session = Session {
                    id,
                    uid: paths::uid("s"),
                    scratch_dir: None,
                    title: title.clone(),
                    display_title: title,
                    logo: logos::default_logo(),
                    pane_tree: Some(PaneNode::Leaf(id)),
                    maximized_pane_id: None,
                    panes: vec![pane],
                    status: SessionStatus::Idle,
                };
                self.projects.push(Project {
                    id,
                    name: SharedString::from(name),
                    path: default_dir,
                    projectless: false,
                    sessions: vec![session],
                    active_session_idx: 0,
                    expanded: true,
                    visible_session_count: 5,
                });
                self.focused_pane_id = Some(id);
                self.active_project_idx = 0;
            }
        } else {
            self.focused_pane_id = self.active_session().and_then(|session| {
                focus_by_session
                    .get(&session.id)
                    .copied()
                    .flatten()
                    .filter(|id| session.panes.iter().any(|pane| pane.id == *id))
                    .or_else(|| session.panes.first().map(|pane| pane.id))
            });
        }

        self.rebuild_session_index();
        let restored_changes: Vec<_> = self
            .projects
            .iter()
            .flat_map(|project| &project.sessions)
            .flat_map(|session| &session.panes)
            .filter(|pane| pane.tool_panel && pane.diff_preview.is_some())
            .map(|pane| pane.id)
            .collect();
        for pane_id in restored_changes {
            self.refresh_project_diff(pane_id, cx);
        }
        if let Some(pane_id) = self.focused_pane_id
            && let Some(pane) = self.pane_ref(pane_id)
        {
            let dir = self.agent_working_directory(pane);
            self.schedule_git_refresh(dir, cx);
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    pub fn tokens(&self, cx: &App) -> ThemeTokens {
        let mode = gpui_component::Theme::global(cx).mode;
        self.settings.theme_colors.tokens(mode, self.palette_scheme)
    }

    #[allow(dead_code)]
    pub fn tokens_with_appearance(&self, appearance: gpui::WindowAppearance) -> ThemeTokens {
        let mode = self.theme_preference.resolve_mode(appearance);
        self.settings.theme_colors.tokens(mode, self.palette_scheme)
    }

    /// Switch the desktop and terminal palette.
    pub fn set_palette_scheme(
        &mut self,
        scheme: PaletteScheme,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if self.palette_scheme == scheme {
            return;
        }
        self.settings
            .theme_colors
            .change_scheme(self.palette_scheme, scheme);
        self.palette_scheme = scheme;
        if scheme == PaletteScheme::MidnightGarden {
            self.theme_preference = ThemePreference::Dark;
        }
        if self.persistence_path.is_some()
            && let Err(error) = settings::save_atomic(&self.saved_settings())
        {
            eprintln!("failed to save theme: {error}");
        }
        self.apply_theme_preference(window, cx);
        self.schedule_workspace_save(cx);
    }

    pub fn set_theme_preference(
        &mut self,
        preference: ThemePreference,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if self.theme_preference == preference {
            return;
        }
        self.theme_preference = preference;
        if let Err(error) = settings::save_atomic(&self.saved_settings()) {
            eprintln!("failed to save theme preference: {error}");
        }
        self.apply_theme_preference(window, cx);
        self.schedule_workspace_save(cx);
    }

    fn observe_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.observe_window_appearance(window, |state, window, cx| {
            if state.theme_preference == ThemePreference::System {
                state.apply_theme_preference(Some(window), cx);
            }
        })
        .detach();
    }

    pub fn apply_theme_preference(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let appearance = window
            .as_ref()
            .map(|w| w.appearance())
            .unwrap_or_else(|| cx.window_appearance());
        let mode = self.theme_preference.resolve_mode(appearance);
        let is_dark = mode.is_dark();
        self.palette = self
            .settings
            .theme_colors
            .palette(self.palette_scheme, is_dark);
        theme::apply_component_theme(mode, window, cx);
        sync_component_fonts(cx);
        theme::sync_imported_components(self.palette_scheme, self.tokens(cx), cx);

        for project in &mut self.projects {
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    let mut config = pane.terminal.read(cx).config().clone();
                    config.colors = self.palette.clone();
                    pane.terminal.update(cx, |term, cx| {
                        term.update_config(config, cx);
                    });
                }
            }
        }
        cx.notify();
    }

    fn active_project(&self) -> Option<&Project> {
        self.projects.get(self.active_project_idx)
    }

    #[cfg(test)]
    fn active_project_mut(&mut self) -> Option<&mut Project> {
        self.projects.get_mut(self.active_project_idx)
    }

    fn active_session(&self) -> Option<&Session> {
        let project = self.active_project()?;
        project.sessions.get(project.active_session_idx)
    }

    fn one_mode_pane<'a>(&self, session: &'a Session) -> Option<&'a TerminalPane> {
        let panes = || session.panes.iter().filter(|pane| !pane.tool_panel);
        let focused = self.focused_pane_id.and_then(|id| {
            let pane = session.panes.iter().find(|pane| pane.id == id)?;
            Some(pane.preview_owner.filter(|_| pane.tool_panel).unwrap_or(id))
        });
        panes()
            .find(|pane| pane.chat.active && Some(pane.id) == focused)
            .or_else(|| panes().find(|pane| pane.one_mode))
            .or_else(|| panes().find(|pane| Some(pane.id) == self.focused_pane_id))
            .or_else(|| panes().next())
    }

    /// Panes the person can actually see: the selected screen's panes, minus
    /// the siblings a maximized pane hides.
    fn visible_pane_ids(&self) -> HashSet<usize> {
        if self.settings.agent_layout {
            return self
                .active_session()
                .and_then(|session| self.one_mode_pane(session))
                .map(|pane| {
                    let panel = self.active_tool_panel(pane).map(|panel| panel.id);
                    std::iter::once(pane.id).chain(panel).collect()
                })
                .unwrap_or_default();
        }
        self.active_session()
            .map(|session| {
                let max = session.maximized_pane_id;
                session
                    .panes
                    .iter()
                    .filter(|pane| !pane.tool_panel && !pane.one_mode)
                    .filter(|pane| max.is_none_or(|m| m == pane.id))
                    .map(|pane| pane.id)
                    .collect()
            })
            .unwrap_or_default()
    }
}
