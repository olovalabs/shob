impl AppState {
    fn finish_renaming_project(
        &mut self,
        project_id: usize,
        new_name: SharedString,
        cx: &mut Context<Self>,
    ) {
        let trimmed = new_name.trim();
        if !trimmed.is_empty()
            && let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id)
        {
            project.name = SharedString::from(trimmed.to_string());
            self.schedule_workspace_save(cx);
        }
        self.renaming_project = None;
        cx.notify();
    }

    fn cancel_renaming_project(&mut self, cx: &mut Context<Self>) {
        self.renaming_project = None;
        cx.notify();
    }

    /// Open a folder as a project. Opening a folder that is already in the
    /// sidebar focuses that project (and revives it if it has no sessions)
    /// rather than creating a duplicate.
    fn open_project_path(&mut self, folder: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if folder == self.scratch_dir {
            self.choose_new_session_project(None, window, cx);
            return;
        }
        if let Some(existing) = self
            .projects
            .iter()
            .position(|p| !p.projectless && git_workspace::paths_equal(&p.path, &folder))
        {
            if self.projects[existing].sessions.is_empty() {
                self.create_new_session_in_project(existing, window, cx);
            } else {
                self.activate_project(existing, window, cx);
            }
            return;
        }

        let name = folder
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());

        let self_weak = cx.entity().downgrade();
        let next_id = self.next_id;
        self.next_id += 1;

        if let Ok((terminal, pid, generation, terminal_subscription)) = Self::spawn_terminal_view(
            Some(&folder),
            self.palette.clone(),
            self_weak,
            next_id,
            None,
            self.settings.memory.scrollback(),
            cx,
        ) {
            let title = SharedString::from("terminal 1");
            let mut pane = TerminalPane::new(
                next_id,
                generation,
                terminal.clone(),
                title.clone(),
                title.clone(),
                pid,
            );
            pane.chat.selected_model = self.settings.default_agent_model.clone();
            pane.chat.permission_mode = self.settings.permission_mode;
            pane.terminal_subscription = Some(terminal_subscription);
            let (input_state, input_subscription) = build_pane_input(next_id, window, cx);
            input_state.update(cx, |state, cx| state.focus(window, cx));
            pane.input_state = Some(input_state);
            pane.input_subscription = Some(input_subscription);
            let session = Session {
                id: next_id,
                uid: paths::uid("s"),
                scratch_dir: None,
                title: title.clone(),
                display_title: title,
                logo: logos::default_logo(),
                pane_tree: Some(PaneNode::Leaf(next_id)),
                maximized_pane_id: None,
                panes: vec![pane],
                status: SessionStatus::Idle,
            };
            let project = Project {
                id: next_id,
                name: SharedString::from(name),
                path: folder,
                projectless: false,
                sessions: vec![session],
                active_session_idx: 0,
                expanded: true,
                visible_session_count: 5,
            };

            self.projects.push(project);
            self.active_project_idx = self.projects.len() - 1;
            self.rebuild_session_index();
            self.focused_pane_id = Some(next_id);
            if self.settings.agent_layout {
                self.focus_one_chat(next_id, window, cx);
            } else if let Some(pane) = self.pane_ref(next_id) {
                let dir = self.agent_working_directory(pane);
                self.schedule_git_refresh(dir, cx);
            }
            cx.notify();
            self.schedule_workspace_save(cx);
        }
    }

    fn select_folder_and_add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            self.open_project_path(folder, window, cx);
        }
    }

    fn rebuild_session_index(&mut self) {
        self.session_index.clear();
        self.pane_index.clear();
        for (p_idx, project) in self.projects.iter().enumerate() {
            for (s_idx, session) in project.sessions.iter().enumerate() {
                self.session_index.insert(session.id, (p_idx, s_idx));
                for (pane_idx, pane) in session.panes.iter().enumerate() {
                    self.pane_index.insert(pane.id, (p_idx, s_idx, pane_idx));
                }
            }
        }
    }

    /// Split a pane inside its session's own screen: the new terminal becomes
    /// a sibling leaf in that session's layout tree. Other sessions' screens
    /// are untouched.
    fn split_pane(
        &mut self,
        target_pane_id: usize,
        axis: gpui::Axis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let self_weak = cx.entity().downgrade();
        let split_id = self.next_id;
        self.next_id += 1;
        let next_id = self.next_id;
        self.next_id += 1;
        let Some(&(p_idx, s_idx, _)) = self.pane_index.get(&target_pane_id) else {
            return;
        };

        let project = &self.projects[p_idx];
        let session = &project.sessions[s_idx];
        let dir = project.session_dir(session);
        let scratch_cwd = session.scratch_dir.clone();
        let pane_count = session.panes.len();
        if let Ok((terminal, pid, generation, terminal_subscription)) = Self::spawn_terminal_view(
            Some(dir),
            self.palette.clone(),
            self_weak,
            next_id,
            None,
            self.settings.memory.scrollback(),
            cx,
        ) {
            let title = SharedString::from(format!("terminal {}", pane_count + 1));
            let display_title = truncate_title(&title);
            let mut pane = TerminalPane::new(
                next_id,
                generation,
                terminal.clone(),
                title.clone(),
                display_title,
                pid,
            );
            pane.cwd = scratch_cwd;
            pane.one_mode = self
                .pane_ref(target_pane_id)
                .is_some_and(|pane| pane.one_mode);
            pane.chat.selected_model = self.settings.default_agent_model.clone();
            pane.chat.permission_mode = self.settings.permission_mode;
            pane.terminal_subscription = Some(terminal_subscription);
            let (input_state, input_subscription) = build_pane_input(next_id, window, cx);
            input_state.update(cx, |state, cx| state.focus(window, cx));
            pane.input_state = Some(input_state);
            pane.input_subscription = Some(input_subscription);

            if let Some(project) = self.projects.get_mut(p_idx)
                && let Some(session) = project.sessions.get_mut(s_idx)
            {
                let one_mode = pane.one_mode;
                session.panes.push(pane);
                if !one_mode {
                    session.maximized_pane_id = None;
                }
                if let Some(tree) = &mut session.pane_tree {
                    tree.split(target_pane_id, next_id, split_id, axis);
                } else {
                    session.pane_tree = Some(PaneNode::Leaf(next_id));
                }
            }
            self.focused_pane_id = Some(next_id);
            self.rebuild_session_index();
            self.schedule_workspace_save(cx);
            cx.notify();
        }
    }

    /// Find or create the one preview owned by a pane. Links opened from a
    /// preview replace that preview, keeping the original agent pane intact.
    fn prepare_preview_pane(
        &mut self,
        source: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if let Some(owner) = self.tool_panel_owner(source) {
            let panel = self
                .tool_panel_panes(owner)
                .find(|pane| {
                    pane.files_surface.is_none()
                        && (pane.diff_preview.is_some() || pane.markdown_preview.is_some())
                })
                .map(|pane| pane.id);
            let id = match panel {
                Some(id) => id,
                None => self.create_tool_panel_tab(owner, window, cx)?,
            };
            let pane = self.pane_mut(id)?;
            pane.preview_generation = pane.preview_generation.wrapping_add(1);
            pane.markdown_preview = None;
            pane.diff_preview = None;
            self.select_tool_panel_tab(owner, id, window, cx);
            return Some(id);
        }
        let &(project, session, _) = self.pane_index.get(&source)?;
        let screen = &self.projects[project].sessions[session];
        let existing = if screen
            .panes
            .iter()
            .any(|pane| pane.id == source && pane.preview_owner.is_some())
        {
            Some(source)
        } else {
            screen
                .panes
                .iter()
                .find(|pane| !pane.tool_panel && pane.preview_owner == Some(source))
                .map(|pane| pane.id)
        };
        let preview = if let Some(id) = existing {
            id
        } else {
            let source_on_right = screen
                .pane_tree
                .as_ref()
                .and_then(|tree| tree.preview_goes_left(source))
                .unwrap_or(false);
            let id = self.next_id + 1;
            self.split_pane(source, gpui::Axis::Horizontal, window, cx);
            let pane = self.pane_mut(id)?;
            pane.preview_owner = Some(source);
            if source_on_right
                && let Some(tree) = self.projects[project].sessions[session].pane_tree.as_mut()
            {
                tree.swap_split_children(source, id);
            }
            id
        };
        if !self.pane_ref(source).is_some_and(|pane| pane.one_mode) {
            self.projects[project].sessions[session].maximized_pane_id = None;
        }
        let pane = self.pane_mut(preview)?;
        pane.preview_generation = pane.preview_generation.wrapping_add(1);
        pane.markdown_preview = None;
        pane.diff_preview = None;
        pane.files_active = false;
        self.focused_pane_id = Some(preview);
        cx.notify();
        Some(preview)
    }

    /// Route local links to the opposite pane; external URLs keep their normal handler.
    fn open_local_link(
        &mut self,
        source: usize,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pane) = self.pane_ref(source) else {
            return false;
        };
        let Some(base) = pane
            .markdown_preview
            .as_ref()
            .and_then(|(path, _)| path.parent().map(Path::to_path_buf))
            .or_else(|| pane.chat.cwd.as_ref().map(PathBuf::from))
            .or_else(|| self.pane_cwd_path(pane))
        else {
            return false;
        };
        let Some(path) = file_preview::local_link_path(target, &base) else {
            return false;
        };
        if file_preview::is_markdown_path(&path)
            && self.open_markdown_preview(source, &path.to_string_lossy(), window, cx)
        {
            return true;
        }
        let Some(root) = self.pane_session_dir(source).cloned() else {
            return false;
        };
        let display_path = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let Some(new_id) = self.prepare_preview_pane(source, window, cx) else {
            return true;
        };
        self.show_read_only_file(new_id, root, display_path, window, cx);
        true
    }

    fn show_read_only_file(
        &mut self,
        new_id: usize,
        root: PathBuf,
        display_path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(new_id) else {
            return;
        };
        pane.display_title = file_name(&display_path).to_owned().into();
        pane.diff_preview = Some(DiffPreview {
            directory: root.clone(),
            result: Some(Ok(project_diff::ProjectDiff {
                root,
                files: Vec::new(),
            })),
            expanded: HashSet::new(),
            tracked_collapsed: false,
            untracked_collapsed: false,
            request: 0,
            file_request: 0,
            file_view: None,
            file_only: true,
        });
        self.open_changed_file(new_id, display_path, window, cx);
        self.schedule_workspace_save(cx);
    }

    /// Open a local Markdown link beside its source, leaving the source running.
    fn open_markdown_preview(
        &mut self,
        source: usize,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let path = if let Ok(url) = url::Url::parse(target) {
            if url.scheme() != "file" {
                return false;
            }
            match url.to_file_path() {
                Ok(path) => path,
                Err(_) => return false,
            }
        } else {
            PathBuf::from(target)
        };
        if !file_preview::is_markdown_path(&path) {
            return false;
        }
        let Some(root) = self.pane_session_dir(source) else {
            return false;
        };
        let Some(pane) = self.pane_ref(source) else {
            return false;
        };
        let path = if path.is_absolute() {
            path
        } else {
            pane.markdown_preview
                .as_ref()
                .and_then(|(path, _)| path.parent().map(|p| p.to_path_buf()))
                .or_else(|| self.pane_cwd_path(pane))
                .unwrap_or_else(|| root.clone())
                .join(path)
        };
        let Some(new_id) = self.prepare_preview_pane(source, window, cx) else {
            return false;
        };
        self.load_markdown_preview(new_id, path, cx);
        true
    }

    fn load_markdown_preview(&mut self, new_id: usize, path: PathBuf, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(new_id) else {
            return;
        };
        let generation = pane.preview_generation;
        pane.markdown_preview = Some((path.clone(), "<p>Loading…</p>".into()));
        pane.display_title = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
            .into();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { file_preview::read_markdown(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(preview) = this
                    .pane_mut(new_id)
                    .filter(|pane| pane.preview_generation == generation)
                    .and_then(|pane| pane.markdown_preview.as_mut())
                {
                    preview.1 = result.unwrap_or_else(str::to_owned).into();
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
        self.schedule_workspace_save(cx);
    }

    fn open_project_diff(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.pane_session_dir(source) else {
            return;
        };
        let directory = self
            .pane_ref(source)
            .filter(|pane| pane.chat.active)
            .map(|pane| self.agent_working_directory(pane))
            .unwrap_or_else(|| root.clone());
        let Some(new_id) = self.prepare_preview_pane(source, window, cx) else {
            return;
        };
        let Some(pane) = self.pane_mut(new_id) else {
            return;
        };
        pane.display_title = "Project changes".into();
        pane.diff_preview = Some(DiffPreview {
            directory,
            result: None,
            expanded: HashSet::new(),
            tracked_collapsed: false,
            untracked_collapsed: false,
            request: 0,
            file_request: 0,
            file_view: None,
            file_only: false,
        });
        self.refresh_project_diff(new_id, cx);
        self.schedule_workspace_save(cx);
    }

    fn build_files_surface(
        &self,
        pane_id: usize,
        root: PathBuf,
        saved: files_surface::SavedFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<files_surface::FilesSurface>, gpui::Subscription) {
        let owner = cx.entity().downgrade();
        let view =
            cx.new(|cx| files_surface::FilesSurface::new(root, saved, owner, pane_id, window, cx));
        let subscription = cx.subscribe(&view, |this, _, _: &files_surface::Changed, cx| {
            this.schedule_workspace_save(cx);
        });
        (view, subscription)
    }

    fn open_files_surface(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.pane_session_dir(source) else {
            return;
        };
        let root = self
            .pane_ref(source)
            .filter(|pane| pane.chat.active)
            .map(|pane| self.agent_working_directory(pane))
            .unwrap_or_else(|| root.clone());
        let panel_owner = self.tool_panel_owner(source);
        let Some(id) = (if let Some(owner) = panel_owner {
            let existing = self
                .tool_panel_panes(owner)
                .find(|pane| pane.files_surface.is_some() || pane.files_restore.is_some())
                .map(|pane| pane.id);
            existing.or_else(|| self.create_tool_panel_tab(owner, window, cx))
        } else {
            self.prepare_preview_pane(source, window, cx)
        }) else {
            return;
        };
        if self
            .pane_ref(id)
            .is_some_and(|pane| pane.files_surface.is_none())
        {
            let view = self.build_files_surface(
                id,
                root,
                files_surface::SavedFiles::default(),
                window,
                cx,
            );
            self.pane_mut(id).unwrap().files_surface = Some(view);
        }
        if let Some(pane) = self.pane_mut(id) {
            if pane.diff_preview.is_some() || pane.markdown_preview.is_some() {
                pane.preview_generation = pane.preview_generation.wrapping_add(1);
                pane.diff_preview = None;
                pane.markdown_preview = None;
            }
            pane.files_active = true;
            pane.display_title = "Files".into();
        }
        if let Some(owner) = panel_owner {
            self.select_tool_panel_tab(owner, id, window, cx);
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn new_browser_pane(
        id: usize,
        owner: usize,
        dir: PathBuf,
        saved: workspace_state::SavedBrowser,
        cx: &mut Context<Self>,
    ) -> TerminalPane {
        // Keep the pane tree's shape without creating a PTY or a shell input.
        let terminal = cx.new(|cx| {
            TerminalView::new(
                std::io::sink(),
                std::io::empty(),
                TerminalConfig {
                    cols: 2,
                    rows: 2,
                    scrollback: 0,
                    read_only: true,
                    ..TerminalConfig::default()
                },
                cx,
            )
        });
        let mut pane = TerminalPane::new(
            id,
            status_bus::new_generation(),
            terminal,
            "browser".into(),
            "Browser".into(),
            None,
        );
        pane.cwd = Some(dir);
        pane.tool_panel = true;
        pane.preview_owner = Some(owner);
        pane.browser = Some(ToolPanelBrowser::Saved(saved));
        pane
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn open_tool_panel_browser(&mut self, owner: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.ensure_tool_panel_browser(owner, cx) {
            self.select_tool_panel_tab(owner, id, window, cx);
        }
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn ensure_tool_panel_browser(&mut self, owner: usize, cx: &mut Context<Self>) -> Option<usize> {
        if self.tool_panel_owner(owner) != Some(owner) {
            return None;
        }
        let existing = self
            .tool_panel_panes(owner)
            .find(|pane| pane.is_browser())
            .map(|pane| pane.id);
        if let Some(id) = existing {
            return Some(id);
        }
        let (project, session, _) = self.pane_index[&owner];
        let split_id = self.next_id;
        let id = split_id + 1;
        self.next_id += 2;
        let pane = Self::new_browser_pane(
            id,
            owner,
            self.projects[project].session_dir(&self.projects[project].sessions[session]).clone(),
            workspace_state::SavedBrowser::default(),
            cx,
        );
        let session = &mut self.projects[project].sessions[session];
        session.panes.push(pane);
        if let Some(tree) = &mut session.pane_tree {
            tree.split(owner, id, split_id, gpui::Axis::Horizontal);
        }
        self.rebuild_session_index();
        Some(id)
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn mount_browser<'a>(
        pane: &'a mut TerminalPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<&'a MountedBrowser> {
        if let ToolPanelBrowser::Saved(saved) = pane.browser.as_mut()? {
            let saved = std::mem::take(saved);
            let view = cx.new(|cx| crate::browser_surface::BrowserSurface::new(saved, window, cx));
            let subscription =
                cx.subscribe(&view, |this, _, _: &crate::browser_surface::Changed, cx| {
                    this.schedule_workspace_save(cx);
                });
            pane.browser = Some(ToolPanelBrowser::Mounted(MountedBrowser {
                view,
                focus: cx.focus_handle(),
                _subscription: subscription,
            }));
        }
        pane.mounted_browser()
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn active_browser_pane(&self) -> Option<&TerminalPane> {
        if !self.startup_loaded
            || self.active_view != ActiveView::Workspace
            || !self.settings.agent_layout
        {
            return None;
        }
        self.active_session()
            .and_then(|session| self.one_mode_pane(session))
            .and_then(|owner| self.active_tool_panel(owner))
            .filter(|pane| pane.is_browser())
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn focused_browser_pane(&self, window: &Window, cx: &App) -> Option<usize> {
        let pane = self.active_browser_pane()?;
        pane.mounted_browser()?
            .focus
            .contains_focused(window, cx)
            .then_some(pane.id)
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn sync_browser_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_browser_pane();
        let mounted_id = pane
            .and_then(TerminalPane::mounted_browser)
            .map(|browser| browser.view.entity_id());
        if self.visible_browser.as_ref().map(|view| view.entity_id()) == mounted_id
            && (mounted_id.is_some() || pane.is_none())
        {
            return;
        }
        let pane_id = pane.map(|pane| pane.id);
        if let Some(view) = self.visible_browser.take().and_then(|view| view.upgrade()) {
            view.update(cx, |view, cx| view.set_mounted(false, window, cx));
        }
        if let Some(id) = pane_id {
            let focus_new = mounted_id.is_none() && self.focused_pane_id == Some(id);
            if let Some(browser) = self
                .pane_mut(id)
                .and_then(|pane| Self::mount_browser(pane, window, cx))
            {
                browser.view.update(cx, |view, cx| {
                    view.set_mounted(true, window, cx);
                    if focus_new {
                        view.focus_address(window, cx);
                    }
                });
                self.visible_browser = Some(browser.view.downgrade());
            }
        }
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn browser_tool_target(
        &mut self,
        owner: usize,
        generation: &str,
        opened: &mut bool,
        command: &crate::browser_automation::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Option<Entity<crate::browser_surface::BrowserSurface>>, &'static str> {
        use crate::browser_automation::Command;

        if !self.startup_loaded
            || self.active_view != ActiveView::Workspace
            || !self.settings.agent_layout
            || self
                .active_session()
                .and_then(|session| self.one_mode_pane(session))
                .is_none_or(|pane| {
                    pane.id != owner
                        || pane.generation != generation
                        || !pane.chat.active
                        || !pane.chat.running
                        || pane.diff_preview.is_some()
                        || pane.markdown_preview.is_some()
                })
        {
            return Err("The requesting thread is not active. Return to it to use the Browser pane.");
        }
        let mut visible = self
            .active_browser_pane()
            .filter(|pane| pane.preview_owner == Some(owner))
            .map(|pane| pane.id);
        if visible.is_none() {
            if matches!(command, Command::List) && !*opened {
                return Ok(None);
            }
            if *opened || !matches!(command, Command::Open { .. }) {
                return Err("The Browser pane is hidden. Show it before using browser tools.");
            }
            let id = self
                .ensure_tool_panel_browser(owner, cx)
                .ok_or("This thread cannot open a Browser pane.")?;
            self.show_tool_panel_tab(owner, id);
            self.schedule_workspace_save(cx);
            cx.notify();
            visible = Some(id);
        }
        self.sync_browser_visibility(window, cx);
        let view = visible
            .and_then(|id| self.pane_ref(id))
            .and_then(TerminalPane::mounted_browser)
            .ok_or("The Browser pane could not be shown.")?
            .view
            .clone();
        *opened = true;
        Ok(Some(view))
    }

    fn tool_panel_owner(&self, source: usize) -> Option<usize> {
        if !self.settings.agent_layout {
            return None;
        }
        let &(project, session, _) = self.pane_index.get(&source)?;
        let owner = self.one_mode_pane(&self.projects[project].sessions[session])?;
        let pane = self.pane_ref(source)?;
        (owner.chat.active
            && (pane.id == owner.id || (pane.tool_panel && pane.preview_owner == Some(owner.id))))
        .then_some(owner.id)
    }

    fn tool_panel_panes(&self, owner: usize) -> impl Iterator<Item = &TerminalPane> {
        self.pane_index
            .get(&owner)
            .into_iter()
            .flat_map(move |&(project, session, _)| &self.projects[project].sessions[session].panes)
            .filter(move |pane| pane.tool_panel && pane.preview_owner == Some(owner))
    }

    fn selected_tool_panel(&self, owner: &TerminalPane) -> Option<&TerminalPane> {
        self.tool_panel_panes(owner.id)
            .find(|pane| Some(pane.id) == owner.tool_panel_tab)
            .or_else(|| self.tool_panel_panes(owner.id).next())
    }

    fn tool_panel_is_open(&self, owner: &TerminalPane) -> bool {
        !owner.tool_panel_hidden
            && (owner.tool_panel_chooser.is_some() || self.selected_tool_panel(owner).is_some())
    }

    fn active_tool_panel(&self, owner: &TerminalPane) -> Option<&TerminalPane> {
        if owner.tool_panel_hidden
            || owner.tool_panel_chooser.is_some()
            || !owner.chat.active
            || owner.diff_preview.is_some()
            || owner.markdown_preview.is_some()
        {
            return None;
        }
        self.selected_tool_panel(owner)
    }

    fn create_tool_panel_tab(
        &mut self,
        owner: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let &(project, session, _) = self.pane_index.get(&owner)?;
        let maximized = self.projects[project].sessions[session].maximized_pane_id;
        let id = self.next_id + 1;
        self.split_pane(owner, gpui::Axis::Horizontal, window, cx);
        self.projects[project].sessions[session].maximized_pane_id = maximized;
        let pane = self.pane_mut(id)?;
        pane.tool_panel = true;
        pane.preview_owner = Some(owner);
        Some(id)
    }

    fn show_tool_panel_tab(&mut self, owner: usize, id: usize) {
        let position = self.tool_panel_panes(owner).position(|pane| pane.id == id);
        let legacy_ratio = self
            .tool_panel_panes(owner)
            .next()
            .and_then(|pane| pane.tool_panel_chat_ratio);
        if let Some(pane) = self.pane_mut(owner) {
            pane.tool_panel_tab = Some(id);
            pane.tool_panel_hidden = false;
            pane.tool_panel_chooser = None;
            pane.tool_panel_chat_ratio = pane.tool_panel_chat_ratio.or(legacy_ratio);
            if let Some(position) = position {
                pane.tool_panel_scroll
                    .get_or_insert_with(gpui::ScrollHandle::new)
                    .scroll_to_item(position);
            }
        }
    }

    fn select_tool_panel_tab(
        &mut self,
        owner: usize,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_tool_panel_tab(owner, id);
        self.focus_pane(id, window, cx);
        if let Some(view) = self
            .pane_ref(id)
            .filter(|pane| pane.files_active)
            .and_then(|pane| pane.files_surface.as_ref())
            .map(|(view, _)| view.clone())
        {
            window.blur(cx);
            view.update(cx, |view, cx| view.focus_selected(window, cx));
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn new_tool_panel_terminal(
        &mut self,
        owner: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let number = self
            .tool_panel_panes(owner)
            .filter_map(|pane| {
                pane.display_title
                    .strip_prefix("Terminal ")?
                    .parse::<usize>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        if let Some(id) = self.create_tool_panel_tab(owner, window, cx) {
            if let Some(pane) = self.pane_mut(id) {
                pane.display_title = format!("Terminal {number}").into();
            }
            self.select_tool_panel_tab(owner, id, window, cx);
        }
    }

    fn open_tool_panel(&mut self, owner: usize, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self
            .pane_ref(owner)
            .filter(|pane| pane.tool_panel_chooser.is_none())
            .and_then(|pane| self.selected_tool_panel(pane))
            .map(|pane| pane.id);
        if let Some(id) = existing {
            self.select_tool_panel_tab(owner, id, window, cx);
        } else {
            self.show_tool_panel_chooser(owner, window, cx);
        }
    }

    fn show_tool_panel_chooser(
        &mut self,
        owner: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(owner) else {
            return;
        };
        pane.tool_panel_hidden = false;
        pane.tool_panel_chooser
            .get_or_insert_with(|| cx.focus_handle())
            .focus(window, cx);
        self.focused_pane_id = Some(owner);
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn dismiss_tool_panel_chooser(
        &mut self,
        owner: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self
            .pane_ref(owner)
            .and_then(|pane| self.selected_tool_panel(pane))
            .map(|pane| pane.id)
        {
            self.select_tool_panel_tab(owner, id, window, cx);
        } else {
            self.hide_tool_panel(owner, window, cx);
        }
    }

    fn hide_tool_panel(&mut self, owner: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(owner) {
            pane.tool_panel_hidden = true;
        }
        self.focus_pane(owner, window, cx);
        self.schedule_workspace_save(cx);
    }

    fn refresh_project_diff(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let generation = pane.preview_generation;
        let Some(view) = pane.diff_preview.as_mut() else {
            return;
        };
        view.request += 1;
        let request = view.request;
        let directory = view.directory.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_diff::load(directory) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(view) = this
                    .pane_mut(pane_id)
                    .filter(|pane| pane.preview_generation == generation)
                    .and_then(|pane| pane.diff_preview.as_mut())
                    .filter(|view| view.request == request)
                {
                    view.result = Some(result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_changed_file(
        &mut self,
        pane_id: usize,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let generation = pane.preview_generation;
        let Some(view) = pane.diff_preview.as_mut() else {
            return;
        };
        let Some(Ok(diff)) = view.result.as_ref() else {
            return;
        };
        let root = diff.root.clone();
        view.file_request += 1;
        let request = view.file_request;
        view.file_view = Some(ReadOnlyFile {
            path: path.clone(),
            state: ReadOnlyFileState::Loading,
        });
        let target = cx.entity().downgrade();
        let handle = window.window_handle();
        cx.notify();
        cx.spawn(async move |_, cx| {
            let read_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { file_preview::read(root, read_path) })
                .await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = target.update(cx, |this, cx| {
                    let focused = this.focused_pane_id == Some(pane_id);
                    let Some(view) = this
                        .pane_mut(pane_id)
                        .filter(|pane| pane.preview_generation == generation)
                        .and_then(|pane| pane.diff_preview.as_mut())
                        .filter(|view| view.file_request == request)
                    else {
                        return;
                    };
                    let Some(file) = view.file_view.as_mut().filter(|file| file.path == path)
                    else {
                        return;
                    };
                    file.state = match result {
                        Ok(content) => {
                            let language = file_preview::language(&path);
                            let input = cx.new(|cx| {
                                InputState::new(window, cx)
                                    .code_editor(language)
                                    .line_number(true)
                                    .soft_wrap(false)
                                    .default_value(content.text)
                                    .line_changes(content.changes)
                                    .read_only(true)
                            });
                            if focused {
                                input.update(cx, |input, cx| input.focus(window, cx));
                            }
                            ReadOnlyFileState::Ready {
                                input,
                                deleted: content.deleted,
                            }
                        }
                        Err(error) => ReadOnlyFileState::Error(error),
                    };
                    cx.notify();
                });
            });
        })
        .detach();
    }
}
