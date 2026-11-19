use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
enum BranchPickerItem {
    Existing(git_workspace::BranchInfo),
    Create(String),
}

fn filtered_branch_items(
    snapshot: Option<&git_workspace::GitWorkspaceSnapshot>,
    query: &str,
) -> Vec<BranchPickerItem> {
    let Some(snapshot) = snapshot else {
        let sanitized = git_workspace::sanitize_new_ref_name(query);
        return if sanitized.is_empty() {
            Vec::new()
        } else {
            vec![BranchPickerItem::Create(sanitized)]
        };
    };

    let trimmed = query.trim();
    let needle = trimmed.to_lowercase();
    let mut items: Vec<BranchPickerItem> = snapshot
        .branches
        .iter()
        .filter(|branch| {
            needle.is_empty() || branch.name.to_lowercase().contains(&needle)
        })
        .cloned()
        .map(BranchPickerItem::Existing)
        .collect();

    if !trimmed.is_empty() {
        let sanitized = git_workspace::sanitize_new_ref_name(trimmed);
        let exact_local_exists = snapshot
            .branches
            .iter()
            .any(|b| !b.is_remote && b.name == sanitized);
        if !sanitized.is_empty() && !exact_local_exists {
            items.push(BranchPickerItem::Create(sanitized));
        }
    }

    items
}

impl AppState {
    pub(crate) fn close_workspace_popovers(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane_mut(pane_id) {
            let changed = pane.workspace_menu_open
                || pane.project_menu_open
                || pane.branch_picker.is_some();
            pane.workspace_menu_open = false;
            pane.project_menu_open = false;
            pane.branch_picker = None;
            if changed {
                cx.notify();
            }
        }
    }

    pub(crate) fn schedule_git_refresh(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !dir.is_dir() || !self.git_refreshing.insert(dir.clone()) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let inspect_dir = dir.clone();
            let snapshot = cx
                .background_executor()
                .spawn(async move { git_workspace::inspect(&inspect_dir) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.git_refreshing.remove(&dir);
                this.apply_git_snapshot(dir, snapshot, cx);
            });
        })
        .detach();
    }

    pub(crate) fn apply_git_snapshot(
        &mut self,
        dir: PathBuf,
        snapshot: git_workspace::GitWorkspaceSnapshot,
        cx: &mut Context<Self>,
    ) -> bool {
        let previous = self.git_snapshots.get(&dir).cloned();
        let snapshot_changed = previous.as_ref() != Some(&snapshot);
        if previous
            .as_ref()
            .is_some_and(|prev| prev.current_branch != snapshot.current_branch)
        {
            git_workspace::invalidate_branch_cache(&dir);
        }
        self.git_snapshots.insert(dir, snapshot);

        let mut reconciled = false;
        let mut reset_panes = Vec::new();
        for project in &mut self.projects {
            let project_path = project.path.clone();
            for session in &mut project.sessions {
                for pane in &mut session.panes {
                    if let Some(override_path) = pane.chat.workspace_override.as_ref()
                        && !override_path.is_dir()
                        && !pane.chat.running
                    {
                        pane.chat.workspace_override = None;
                        pane.chat.bridge = None;
                        pane.chat.bridge_model = None;
                        pane.chat.bridge_resume = None;
                        pane.chat.rpi_session_id = None;
                        pane.chat.cwd = Some(project_path.clone());
                        pane.chat.bridge_needs_context = !pane.chat.messages.is_empty();
                        reset_panes.push(pane.id);
                        reconciled = true;
                    }
                }
            }
        }

        if reconciled {
            self.schedule_workspace_save(cx);
        }
        if !reset_panes.is_empty()
            && let Some(active_dir) = self
                .focused_pane_id
                .and_then(|id| self.pane_ref(id))
                .map(|pane| self.agent_working_directory(pane))
        {
            self.schedule_git_refresh(active_dir, cx);
        }
        if snapshot_changed || reconciled {
            cx.notify();
        }
        snapshot_changed || reconciled
    }

    pub(crate) fn apply_workspace_override(
        &mut self,
        pane_id: usize,
        new_override: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(project_path) = self
            .pane_index
            .get(&pane_id)
            .and_then(|&(project_idx, _, _)| self.projects.get(project_idx))
            .map(|project| project.path.clone())
        else {
            return;
        };

        let target_dir = new_override.clone().unwrap_or_else(|| project_path.clone());
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if pane.chat.running {
            return;
        }
        let changed = pane.chat.workspace_override != new_override
            || pane.chat.cwd.as_ref() != Some(&target_dir);
        pane.chat.workspace_override = new_override;
        pane.workspace_menu_open = false;
        pane.project_menu_open = false;
        pane.branch_picker = None;
        if changed {
            pane.chat.bridge = None;
            pane.chat.bridge_model = None;
            pane.chat.bridge_resume = None;
            pane.chat.rpi_session_id = None;
            pane.chat.cwd = Some(target_dir.clone());
            pane.chat.bridge_needs_context = !pane.chat.messages.is_empty();
            git_workspace::invalidate_branch_cache(&target_dir);
            self.schedule_workspace_save(cx);
        }
        self.schedule_git_refresh(target_dir, cx);
        cx.notify();
    }

    fn select_current_checkout(
        &mut self,
        pane_id: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        if pane.chat.running {
            return;
        }
        let active_dir = self.agent_working_directory(pane);
        let main_path = self
            .git_snapshots
            .get(&active_dir)
            .and_then(|snap| snap.main_worktree_path.clone());
        let project_path = self
            .pane_index
            .get(&pane_id)
            .and_then(|&(project_idx, _, _)| self.projects.get(project_idx))
            .map(|project| project.path.clone());

        let new_override = match (main_path, project_path) {
            (Some(main), Some(proj)) if !git_workspace::paths_equal(&main, &proj) && main.is_dir() => {
                Some(main)
            }
            _ => None,
        };
        self.apply_workspace_override(pane_id, new_override, cx);
    }

    fn select_existing_worktree(
        &mut self,
        pane_id: usize,
        worktree_path: PathBuf,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !worktree_path.is_dir() {
            return;
        }
        let project_path = self
            .pane_index
            .get(&pane_id)
            .and_then(|&(project_idx, _, _)| self.projects.get(project_idx))
            .map(|project| project.path.clone());
        let new_override = if project_path
            .as_ref()
            .is_some_and(|proj| git_workspace::paths_equal(proj, &worktree_path))
        {
            None
        } else {
            Some(worktree_path)
        };
        self.apply_workspace_override(pane_id, new_override, cx);
    }

    fn create_and_switch_worktree(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        if pane.chat.running || self.git_worktree_creating.contains(&pane_id) {
            return;
        }
        let source_dir = self.agent_working_directory(pane);
        let base_branch = self
            .git_snapshots
            .get(&source_dir)
            .and_then(|snap| snap.current_branch.clone());

        if let Some(pane) = self.pane_mut(pane_id) {
            pane.workspace_menu_open = false;
            pane.branch_picker = None;
            pane.project_menu_open = false;
        }
        self.git_worktree_creating.insert(pane_id);
        cx.notify();

        let worktrees_root = paths::worktrees_dir();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let wt = git_workspace::create_worktree(
                        &source_dir,
                        &worktrees_root,
                        base_branch.as_deref(),
                    )?;
                    let wt_snapshot = git_workspace::inspect(&wt.path);
                    let src_snapshot = git_workspace::inspect(&source_dir);
                    Ok::<_, String>((source_dir, src_snapshot, wt, wt_snapshot))
                })
                .await;

            let _ = this.update_in(cx, |this, window, cx| {
                this.git_worktree_creating.remove(&pane_id);
                match result {
                    Ok((source_dir, src_snapshot, wt, wt_snapshot)) => {
                        this.apply_git_snapshot(source_dir, src_snapshot, cx);
                        this.apply_git_snapshot(wt.path.clone(), wt_snapshot, cx);
                        this.apply_workspace_override(pane_id, Some(wt.path), cx);
                    }
                    Err(error) => {
                        window.push_notification(
                            Notification::new()
                                .title("Could not create worktree")
                                .message(error)
                                .with_type(NotificationType::Error),
                            cx,
                        );
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn open_branch_picker(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        if pane.branch_picker.is_some() {
            self.close_branch_picker(pane_id, window, cx);
            return;
        }

        let active_dir = self.agent_working_directory(pane);
        self.schedule_git_refresh(active_dir, cx);

        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search or create branch..."));
        filter.update(cx, |state, cx| state.focus(window, cx));
        let subscription = cx.subscribe_in(
            &filter,
            window,
            move |this: &mut Self, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    if let Some(pane) = this.pane_mut(pane_id)
                        && let Some(picker) = pane.branch_picker.as_mut()
                    {
                        picker.selected = 0;
                        cx.notify();
                    }
                } else if let InputEvent::PressEnter { .. } = event {
                    this.confirm_branch_picker_selection(pane_id, window, cx);
                }
            },
        );

        if let Some(pane) = self.pane_mut(pane_id) {
            pane.workspace_menu_open = false;
            pane.project_menu_open = false;
            pane.permission_menu_open = false;
            pane.plus_menu_open = false;
            pane.branch_picker = Some(BranchPickerState {
                input: filter,
                selected: 0,
                _subscription: subscription,
            });
            cx.notify();
        }
    }

    fn close_branch_picker(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self.pane_mut(pane_id)
            && pane.branch_picker.take().is_some()
        {
            if pane.should_show_bottom_input(cx)
                && let Some(input_state) = &pane.input_state
            {
                input_state.update(cx, |state, cx| state.focus(window, cx));
            }
            cx.notify();
        }
    }

    fn move_branch_picker_selection(&mut self, pane_id: usize, delta: isize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let Some(picker) = pane.branch_picker.as_ref() else {
            return;
        };
        let query = picker.input.read(cx).value().to_string();
        let active_dir = self.agent_working_directory(pane);
        let items = filtered_branch_items(self.git_snapshots.get(&active_dir), &query);
        if items.is_empty() {
            return;
        }
        let len = items.len() as isize;
        let next = (picker.selected.min(items.len() - 1) as isize + delta).rem_euclid(len) as usize;
        if let Some(pane) = self.pane_mut(pane_id)
            && let Some(picker) = pane.branch_picker.as_mut()
        {
            picker.selected = next;
            cx.notify();
        }
    }

    fn confirm_branch_picker_selection(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected) = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.branch_picker.as_ref())
            .map(|picker| picker.selected)
        else {
            return;
        };
        self.select_branch_item(pane_id, selected, window, cx);
    }

    fn select_branch_item(
        &mut self,
        pane_id: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let Some(picker) = pane.branch_picker.as_ref() else {
            return;
        };
        if pane.chat.running {
            window.push_notification(
                Notification::new()
                    .title("Agent turn in progress")
                    .message("Stop the current turn before switching branches.")
                    .with_type(NotificationType::Warning),
                cx,
            );
            return;
        }
        let query = picker.input.read(cx).value().to_string();
        let active_dir = self.agent_working_directory(pane);
        let snapshot = self.git_snapshots.get(&active_dir);
        let items = filtered_branch_items(snapshot, &query);
        let Some(item) = items.get(index).cloned() else {
            return;
        };

        self.close_branch_picker(pane_id, window, cx);

        match item {
            BranchPickerItem::Existing(branch) => {
                if let Some(worktree_path) = branch.worktree_path.as_ref()
                    && !git_workspace::paths_equal(worktree_path, &active_dir)
                    && worktree_path.is_dir()
                {
                    self.select_existing_worktree(pane_id, worktree_path.clone(), window, cx);
                    return;
                }
                let branch_name = branch.name;
                let is_remote = branch.is_remote;
                cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            git_workspace::checkout_branch(&active_dir, &branch_name, is_remote)?;
                            let updated = git_workspace::inspect(&active_dir);
                            Ok::<_, String>((active_dir, updated))
                        })
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| match result {
                        Ok((dir, updated)) => {
                            git_workspace::invalidate_branch_cache(&dir);
                            this.apply_git_snapshot(dir, updated, cx);
                        }
                        Err(error) => {
                            window.push_notification(
                                Notification::new()
                                    .title("Could not switch branch")
                                    .message(error)
                                    .with_type(NotificationType::Error),
                                cx,
                            );
                        }
                    });
                })
                .detach();
            }
            BranchPickerItem::Create(raw_name) => {
                cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            git_workspace::create_and_checkout_branch(&active_dir, &raw_name)?;
                            let updated = git_workspace::inspect(&active_dir);
                            Ok::<_, String>((active_dir, updated))
                        })
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| match result {
                        Ok((dir, updated)) => {
                            git_workspace::invalidate_branch_cache(&dir);
                            this.apply_git_snapshot(dir, updated, cx);
                        }
                        Err(error) => {
                            window.push_notification(
                                Notification::new()
                                    .title("Could not create branch")
                                    .message(error)
                                    .with_type(NotificationType::Error),
                                cx,
                            );
                        }
                    });
                })
                .detach();
            }
        }
    }

    fn switch_agent_project(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane_id) = self.focused_pane_id {
            self.close_workspace_popovers(pane_id, cx);
        }
        let Some(project) = self.projects.get(project_idx) else {
            return;
        };
        if project.sessions.is_empty() {
            self.create_project_session(project_idx, true, window, cx);
        } else {
            self.activate_project(project_idx, window, cx);
        }
        if let Some(pane) = self
            .focused_pane_id
            .and_then(|id| self.pane_ref(id))
        {
            let dir = self.agent_working_directory(pane);
            self.schedule_git_refresh(dir, cx);
        }
    }

    fn open_project_folder_from_switcher(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_workspace_popovers(pane_id, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await else {
                return;
            };
            let path = folder.path().to_path_buf();
            let _ = this.update_in(cx, |this, window, cx| {
                let existing = this
                    .projects
                    .iter()
                    .position(|p| git_workspace::paths_equal(&p.path, &path));
                this.open_project_path(path, window, cx);
                if existing.is_none()
                    && let Some(new_pane_id) = this.focused_pane_id
                {
                    this.set_chat_mode(new_pane_id, true, window, cx);
                }
                if let Some(active_pane) = this
                    .focused_pane_id
                    .and_then(|id| this.pane_ref(id))
                {
                    let dir = this.agent_working_directory(active_pane);
                    this.schedule_git_refresh(dir, cx);
                }
            });
        })
        .detach();
    }

    pub(crate) fn render_composer_context_strip(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        in_pill_footer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pane_id = pane.id;
        let active_dir = self.agent_working_directory(pane);
        let snapshot = self.git_snapshots.get(&active_dir);

        let left_group = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .min_w_0()
            .child(self.render_workspace_selector(pane, &active_dir, snapshot, tokens, window, cx))
            .children(self.render_branch_selector(pane, &active_dir, snapshot, tokens, window, cx));

        let show_usage = in_pill_footer
            && pane.chat.worker_view.is_none()
            && pane.chat.usage.is_some_and(|usage| {
                usage.input != 0
                    || usage.output != 0
                    || usage.cache_read != 0
                    || usage.cache_write != 0
                    || usage.cost.is_some_and(|cost| cost > 0.0)
            });

        let mut right_group = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .flex_shrink_0();

        if show_usage {
            let detail = pane.chat.usage_detail.clone();
            right_group = right_group.child(
                div()
                    .id(("agent-usage", pane_id))
                    .debug_selector(|| "agent-usage".into())
                    .text_size(px(10.0))
                    .text_color(tokens.text_muted)
                    .truncate()
                    .child(pane.chat.usage_label.clone())
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(detail.clone()).build(window, cx)
                    }),
            );
        }

        right_group = right_group.child(self.render_project_selector(pane, tokens, window, cx));

        div()
            .id(("composer-context-strip", pane_id))
            .debug_selector(|| "composer-context-strip".into())
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .when(in_pill_footer, |strip| strip.mt(px(6.0)).min_h(px(22.0)))
            .when(!in_pill_footer, |strip| {
                strip.pt(px(6.0)).px(px(6.0)).min_h(px(24.0))
            })
            .child(left_group)
            .child(right_group)
            .into_any_element()
    }

    fn render_workspace_selector(
        &self,
        pane: &TerminalPane,
        active_dir: &Path,
        snapshot: Option<&git_workspace::GitWorkspaceSnapshot>,
        tokens: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pane_id = pane.id;
        let creating = self.git_worktree_creating.contains(&pane_id);
        let project_path = self
            .pane_index
            .get(&pane_id)
            .and_then(|&(project_idx, _, _)| self.projects.get(project_idx))
            .map(|project| project.path.clone());

        let is_in_worktree = snapshot.is_some_and(|s| s.is_worktree)
            || pane
                .chat
                .workspace_override
                .as_ref()
                .is_some_and(|path| path.starts_with(paths::worktrees_dir()));

        let is_custom_folder = !is_in_worktree
            && pane
                .chat
                .workspace_override
                .as_ref()
                .is_some_and(|override_path| {
                    project_path
                        .as_ref()
                        .is_none_or(|proj| !git_workspace::paths_equal(proj, override_path))
                });

        let (trigger_icon, trigger_label): (&'static str, String) = if creating {
            ("icons/folder-git-2.svg", "Creating worktree…".to_string())
        } else if is_in_worktree {
            ("icons/folder-git.svg", "Current worktree".to_string())
        } else if is_custom_folder {
            let folder_name = active_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "Current checkout".to_string());
            ("icons/folder.svg", folder_name)
        } else {
            ("icons/folder.svg", "Current checkout".to_string())
        };

        let toggle_bounds = pane.workspace_toggle_bounds.clone();
        let outside_bounds = pane.workspace_toggle_bounds.clone();
        let menu_position = {
            let bounds = pane.workspace_toggle_bounds.get();
            gpui::point(bounds.left(), bounds.top())
        };

        let menu_bg = if tokens.is_dark {
            rgba(0x202020ff).into()
        } else {
            tokens.card_bg
        };
        let menu_border = if tokens.is_dark {
            rgba(0x3d3d3dff).into()
        } else {
            tokens.card_border
        };
        let accent: Hsla = if tokens.is_dark {
            rgba(0xf47b37ff).into()
        } else {
            rgba(0xc45113ff).into()
        };
        let hover_bg = tokens.pane_header_btn_hover;

        let is_git_repo = snapshot.is_some_and(|s| s.is_git_repo)
            || git_workspace::detect_git_branch(active_dir).is_some();
        let secondary_worktrees: Vec<git_workspace::WorktreeInfo> = snapshot
            .map(|s| s.secondary_worktrees().into_iter().cloned().collect())
            .unwrap_or_default();
        let active_dir_display = Self::format_path_display(active_dir);
        let can_mutate = !pane.chat.running && !creating;
        let can_new_worktree = can_mutate && is_git_repo;
        let is_local_selected = !is_in_worktree && !is_custom_folder;

        div()
            .relative()
            .child(
                div()
                    .id(("agent-workspace-selector", pane_id))
                    .debug_selector(|| "agent-workspace-selector".into())
                    .relative()
                    .h(px(22.0))
                    .px(px(7.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover_bg))
                    .text_size(px(11.5))
                    .text_color(if is_in_worktree || pane.workspace_menu_open {
                        tokens.text_primary
                    } else {
                        tokens.text_secondary
                    })
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(active_dir_display.clone())
                            .build(window, cx)
                    })
                    .child(
                        canvas(
                            move |bounds, _, _| toggle_bounds.set(bounds),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .child(
                        svg()
                            .path(trigger_icon)
                            .size(px(13.0))
                            .flex_shrink_0()
                            .text_color(if is_in_worktree {
                                accent
                            } else {
                                tokens.text_muted
                            }),
                    )
                    .child(div().truncate().child(trigger_label))
                    .child(
                        svg()
                            .path("icons/chevron-down.svg")
                            .size(px(10.0))
                            .flex_shrink_0()
                            .text_color(tokens.text_muted),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            let refresh_dir = this
                                .pane_ref(pane_id)
                                .map(|pane| this.agent_working_directory(pane));
                            let mut opened = false;
                            if let Some(pane) = this.pane_mut(pane_id) {
                                pane.workspace_menu_open = !pane.workspace_menu_open;
                                opened = pane.workspace_menu_open;
                                pane.project_menu_open = false;
                                pane.branch_picker = None;
                                pane.permission_menu_open = false;
                                pane.plus_menu_open = false;
                            }
                            if opened && let Some(dir) = refresh_dir {
                                this.schedule_git_refresh(dir, cx);
                            }
                            cx.notify();
                        }),
                    ),
            )
            .when(pane.workspace_menu_open, |container| {
                let menu = div()
                    .id(("workspace-popover", pane_id))
                    .debug_selector(|| "agent-workspace-menu".into())
                    .w(px(268.0))
                    .max_w((window.viewport_size().width - px(24.0)).max(px(0.0)))
                    .p(px(5.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(menu_border)
                    .bg(menu_bg)
                    .shadow_lg()
                    .occlude()
                    .text_size(px(12.0))
                    .text_color(tokens.text_primary)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_mouse_down_out(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        if !outside_bounds.get().contains(&event.position)
                            && let Some(pane) = this.pane_mut(pane_id)
                        {
                            pane.workspace_menu_open = false;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .px(px(8.0))
                            .pt(px(4.0))
                            .pb(px(4.0))
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(tokens.text_muted)
                            .child("Workspace"),
                    )
                    .child(
                        div()
                            .id(("workspace-option-local", pane_id))
                            .debug_selector(|| "workspace-option-local".into())
                            .px(px(8.0))
                            .py(px(5.0))
                            .min_h(px(28.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .when(can_mutate, |row| {
                                row.cursor_pointer()
                                    .hover(move |s| s.bg(hover_bg))
                            })
                            .when(!can_mutate, |row| row.opacity(0.5))
                            .child(
                                svg()
                                    .path("icons/folder.svg")
                                    .size(px(14.0))
                                    .flex_shrink_0()
                                    .text_color(if is_local_selected {
                                        accent
                                    } else {
                                        tokens.text_secondary
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(if is_local_selected {
                                        accent
                                    } else {
                                        tokens.text_primary
                                    })
                                    .child("Current checkout"),
                            )
                            .when(is_local_selected, |row| {
                                row.child(
                                    svg()
                                        .path("icons/status-check.svg")
                                        .size(px(13.0))
                                        .flex_shrink_0()
                                        .text_color(accent),
                                )
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.select_current_checkout(pane_id, window, cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id(("workspace-option-new-worktree", pane_id))
                            .debug_selector(|| "workspace-option-new-worktree".into())
                            .px(px(8.0))
                            .py(px(5.0))
                            .min_h(px(28.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .when(can_new_worktree, |row| {
                                row.cursor_pointer()
                                    .hover(move |s| s.bg(hover_bg))
                            })
                            .when(!can_new_worktree, |row| row.opacity(0.5))
                            .child(
                                svg()
                                    .path("icons/folder-git-2.svg")
                                    .size(px(14.0))
                                    .flex_shrink_0()
                                    .text_color(tokens.text_secondary),
                            )
                            .child(div().flex_1().min_w_0().child("New worktree"))
                            .when(!is_git_repo && snapshot.is_some(), |row| {
                                row.child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(tokens.text_muted)
                                        .child("Not Git"),
                                )
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    if can_new_worktree {
                                        this.create_and_switch_worktree(pane_id, window, cx);
                                    }
                                }),
                            ),
                    )
                    .when(!secondary_worktrees.is_empty(), |menu| {
                        menu.child(div().my(px(4.0)).h(px(1.0)).bg(tokens.border_subtle))
                            .child(
                                div()
                                    .px(px(8.0))
                                    .pt(px(2.0))
                                    .pb(px(4.0))
                                    .text_size(px(11.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(tokens.text_muted)
                                    .child("Worktrees"),
                            )
                            .child(
                                div()
                                    .id(("workspace-worktree-list", pane_id))
                                    .max_h(px(180.0))
                                    .overflow_y_scrollbar()
                                    .flex()
                                    .flex_col()
                                    .children(secondary_worktrees.into_iter().enumerate().map(
                                        |(idx, wt)| {
                                            let wt_path = wt.path.clone();
                                            let is_active =
                                                git_workspace::paths_equal(&wt_path, active_dir);
                                            let label = wt.branch.clone().unwrap_or_else(|| {
                                                wt_path
                                                    .file_name()
                                                    .map(|n| n.to_string_lossy().to_string())
                                                    .unwrap_or_else(|| "Worktree".to_string())
                                            });
                                            let sublabel = Self::format_path_display(&wt_path);
                                            div()
                                                .id(("workspace-worktree-item", idx))
                                                .debug_selector(move || {
                                                    format!("workspace-option-worktree-{idx}")
                                                })
                                                .px(px(8.0))
                                                .py(px(5.0))
                                                .rounded(px(6.0))
                                                .flex()
                                                .items_center()
                                                .gap(px(8.0))
                                                .when(can_mutate, |row| {
                                                    row.cursor_pointer().hover(move |s| {
                                                        s.bg(hover_bg)
                                                    })
                                                })
                                                .when(!can_mutate, |row| row.opacity(0.5))
                                                .child(
                                                    svg()
                                                        .path("icons/folder-git.svg")
                                                        .size(px(14.0))
                                                        .flex_shrink_0()
                                                        .text_color(if is_active {
                                                            accent
                                                        } else {
                                                            tokens.text_secondary
                                                        }),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .flex()
                                                        .flex_col()
                                                        .child(
                                                            div()
                                                                .truncate()
                                                                .text_color(if is_active {
                                                                    accent
                                                                } else {
                                                                    tokens.text_primary
                                                                })
                                                                .child(label),
                                                        )
                                                        .child(
                                                            div()
                                                                .truncate()
                                                                .text_size(px(10.0))
                                                                .text_color(tokens.text_muted)
                                                                .child(sublabel),
                                                        ),
                                                )
                                                .when(is_active, |row| {
                                                    row.child(
                                                        svg()
                                                            .path("icons/status-check.svg")
                                                            .size(px(13.0))
                                                            .flex_shrink_0()
                                                            .text_color(accent),
                                                    )
                                                })
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _, window, cx| {
                                                        cx.stop_propagation();
                                                        if can_mutate {
                                                            this.select_existing_worktree(
                                                                pane_id,
                                                                wt_path.clone(),
                                                                window,
                                                                cx,
                                                            );
                                                        }
                                                    }),
                                                )
                                        },
                                    )),
                            )
                    })
                    .when(pane.chat.running, |menu| {
                        menu.child(
                            div()
                                .px(px(8.0))
                                .pt(px(4.0))
                                .pb(px(2.0))
                                .text_size(px(10.5))
                                .text_color(tokens.text_muted)
                                .child("Stop the current turn to switch workspaces."),
                        )
                    });

                container.child(deferred(
                    gpui::anchored()
                        .anchor(gpui::Anchor::BottomLeft)
                        .position(menu_position)
                        .offset(gpui::point(px(0.0), px(-6.0)))
                        .child(menu),
                ))
            })
            .into_any_element()
    }

    fn render_branch_selector(
        &self,
        pane: &TerminalPane,
        active_dir: &Path,
        snapshot: Option<&git_workspace::GitWorkspaceSnapshot>,
        tokens: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let branch_label = snapshot
            .and_then(|s| {
                s.current_branch
                    .clone()
                    .or_else(|| s.head_sha.as_ref().map(|sha| format!("HEAD ({sha})")))
            })
            .or_else(|| git_workspace::detect_git_branch(active_dir))?;

        let pane_id = pane.id;
        let toggle_bounds = pane.branch_toggle_bounds.clone();
        let outside_bounds = pane.branch_toggle_bounds.clone();
        let menu_position = {
            let bounds = pane.branch_toggle_bounds.get();
            gpui::point(bounds.left(), bounds.top())
        };

        let menu_bg = if tokens.is_dark {
            rgba(0x202020ff).into()
        } else {
            tokens.card_bg
        };
        let menu_border = if tokens.is_dark {
            rgba(0x3d3d3dff).into()
        } else {
            tokens.card_border
        };
        let accent: Hsla = if tokens.is_dark {
            rgba(0xf47b37ff).into()
        } else {
            rgba(0xc45113ff).into()
        };
        let git_icon_color: Hsla = if tokens.is_dark {
            rgba(0xadc993ff).into()
        } else {
            rgba(0x2d7a2dff).into()
        };
        let hover_bg = tokens.pane_header_btn_hover;

        let picker_open = pane.branch_picker.is_some();

        Some(
            div()
                .relative()
                .child(
                    div()
                        .id(("agent-branch-selector", pane_id))
                        .debug_selector(|| "agent-branch-selector".into())
                        .relative()
                        .h(px(22.0))
                        .px(px(7.0))
                        .rounded(px(6.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .text_size(px(11.5))
                        .text_color(if picker_open {
                            tokens.text_primary
                        } else {
                            tokens.text_secondary
                        })
                        .child(
                            canvas(
                                move |bounds, _, _| toggle_bounds.set(bounds),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        )
                        .child(
                            svg()
                                .path("icons/git-branch.svg")
                                .size(px(12.0))
                                .flex_shrink_0()
                                .text_color(git_icon_color),
                        )
                        .child(
                            div()
                                .font_family(MONO_FONT)
                                .text_size(px(11.0))
                                .max_w(px(180.0))
                                .truncate()
                                .child(branch_label),
                        )
                        .child(
                            svg()
                                .path("icons/chevron-down.svg")
                                .size(px(10.0))
                                .flex_shrink_0()
                                .text_color(tokens.text_muted),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_branch_picker(pane_id, window, cx);
                            }),
                        ),
                )
                .when_some(pane.branch_picker.as_ref(), |container, picker| {
                    let query = picker.input.read(cx).value().to_string();
                    let items = filtered_branch_items(snapshot, &query);
                    let selected_idx = if items.is_empty() {
                        0
                    } else {
                        picker.selected.min(items.len() - 1)
                    };

                    let menu = div()
                        .id(("branch-popover", pane_id))
                        .debug_selector(|| "agent-branch-menu".into())
                        .w(px(276.0))
                        .max_w((window.viewport_size().width - px(24.0)).max(px(0.0)))
                        .p(px(5.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(menu_border)
                        .bg(menu_bg)
                        .shadow_lg()
                        .occlude()
                        .text_size(px(12.0))
                        .text_color(tokens.text_primary)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_mouse_down_out(cx.listener(
                            move |this, event: &MouseDownEvent, window, cx| {
                                if !outside_bounds.get().contains(&event.position) {
                                    this.close_branch_picker(pane_id, window, cx);
                                }
                            },
                        ))
                        .on_action(cx.listener(
                            move |this, _: &gpui_component::input::Escape, window, cx| {
                                cx.stop_propagation();
                                this.close_branch_picker(pane_id, window, cx);
                            },
                        ))
                        .on_key_down(cx.listener(
                            move |this, event: &KeyDownEvent, window, cx| {
                                match event.keystroke.key.as_str() {
                                    "escape" => {
                                        cx.stop_propagation();
                                        this.close_branch_picker(pane_id, window, cx);
                                    }
                                    "up" => {
                                        cx.stop_propagation();
                                        this.move_branch_picker_selection(pane_id, -1, cx);
                                    }
                                    "down" => {
                                        cx.stop_propagation();
                                        this.move_branch_picker_selection(pane_id, 1, cx);
                                    }
                                    "enter" => {
                                        cx.stop_propagation();
                                        this.confirm_branch_picker_selection(pane_id, window, cx);
                                    }
                                    _ => {}
                                }
                            },
                        ))
                        .child(
                            div()
                                .id(("branch-search-box", pane_id))
                                .debug_selector(|| "agent-branch-search".into())
                                .px(px(6.0))
                                .py(px(4.0))
                                .mb(px(4.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(tokens.border_subtle)
                                .bg(tokens.pane_bg)
                                .child(
                                    Input::new(&picker.input)
                                        .w_full()
                                        .text_size(px(12.0))
                                        .appearance(false)
                                        .bordered(false)
                                        .focus_bordered(false)
                                        .prefix(
                                            svg()
                                                .path("ui_icons/search.svg")
                                                .size(px(13.0))
                                                .text_color(tokens.text_muted),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .id(("branch-picker-list", pane_id))
                                .max_h(px(220.0))
                                .overflow_y_scrollbar()
                                .flex()
                                .flex_col()
                                .when(items.is_empty(), |list| {
                                    list.child(
                                        div()
                                            .px(px(8.0))
                                            .py(px(10.0))
                                            .text_size(px(11.5))
                                            .text_color(tokens.text_muted)
                                            .child("No matching branches"),
                                    )
                                })
                                .children(items.into_iter().enumerate().map(|(idx, item)| {
                                    let is_highlighted = idx == selected_idx;
                                    match item {
                                        BranchPickerItem::Existing(branch) => {
                                            let has_other_worktree = branch
                                                .worktree_path
                                                .as_ref()
                                                .is_some_and(|p| {
                                                    !git_workspace::paths_equal(p, active_dir)
                                                });
                                            let badge = if branch.is_current {
                                                Some("current")
                                            } else if has_other_worktree {
                                                Some("worktree")
                                            } else if branch.is_remote {
                                                Some("remote")
                                            } else if branch.is_default {
                                                Some("default")
                                            } else {
                                                None
                                            };
                                            div()
                                                .id(("branch-option", idx))
                                                .debug_selector(move || {
                                                    format!("branch-option-{idx}")
                                                })
                                                .px(px(8.0))
                                                .py(px(5.0))
                                                .min_h(px(28.0))
                                                .rounded(px(6.0))
                                                .flex()
                                                .items_center()
                                                .gap(px(6.0))
                                                .cursor_pointer()
                                                .when(is_highlighted, |row| {
                                                    row.bg(hover_bg)
                                                })
                                                .hover(move |s| s.bg(hover_bg))
                                                .child(
                                                    svg()
                                                        .path(if has_other_worktree {
                                                            "icons/folder-git.svg"
                                                        } else {
                                                            "icons/git-branch.svg"
                                                        })
                                                        .size(px(13.0))
                                                        .flex_shrink_0()
                                                        .text_color(if branch.is_current {
                                                            accent
                                                        } else {
                                                            tokens.text_muted
                                                        }),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .truncate()
                                                        .font_family(MONO_FONT)
                                                        .text_size(px(11.5))
                                                        .text_color(if branch.is_current {
                                                            accent
                                                        } else {
                                                            tokens.text_primary
                                                        })
                                                        .child(branch.name),
                                                )
                                                .when_some(badge, |row, tag| {
                                                    row.child(
                                                        div()
                                                            .px(px(5.0))
                                                            .py(px(1.0))
                                                            .rounded(px(4.0))
                                                            .bg(tokens.card_hover)
                                                            .text_size(px(10.0))
                                                            .text_color(tokens.text_muted)
                                                            .child(tag),
                                                    )
                                                })
                                                .when(branch.is_current, |row| {
                                                    row.child(
                                                        svg()
                                                            .path("icons/status-check.svg")
                                                            .size(px(12.0))
                                                            .flex_shrink_0()
                                                            .text_color(accent),
                                                    )
                                                })
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _, window, cx| {
                                                        cx.stop_propagation();
                                                        this.select_branch_item(
                                                            pane_id, idx, window, cx,
                                                        );
                                                    }),
                                                )
                                        }
                                        BranchPickerItem::Create(name) => div()
                                            .id(("branch-option-create", idx))
                                            .debug_selector(|| "branch-option-create".into())
                                            .px(px(8.0))
                                            .py(px(5.0))
                                            .min_h(px(28.0))
                                            .rounded(px(6.0))
                                            .flex()
                                            .items_center()
                                            .gap(px(6.0))
                                            .cursor_pointer()
                                            .when(is_highlighted, |row| {
                                                row.bg(hover_bg)
                                            })
                                            .hover(move |s| s.bg(hover_bg))
                                            .child(
                                                svg()
                                                    .path("icons/plus.svg")
                                                    .size(px(13.0))
                                                    .flex_shrink_0()
                                                    .text_color(accent),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(tokens.text_primary)
                                                    .child(format!("Create branch \"{name}\"")),
                                            )
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.select_branch_item(
                                                        pane_id, idx, window, cx,
                                                    );
                                                }),
                                            ),
                                    }
                                })),
                        );

                    container.child(deferred(
                        gpui::anchored()
                            .anchor(gpui::Anchor::BottomLeft)
                            .position(menu_position)
                            .offset(gpui::point(px(0.0), px(-6.0)))
                            .child(menu),
                    ))
                })
                .into_any_element(),
        )
    }

    fn render_project_selector(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pane_id = pane.id;
        let current_project_idx = self
            .pane_index
            .get(&pane_id)
            .map(|&(p_idx, _, _)| p_idx)
            .unwrap_or(self.active_project_idx);
        let project_name = self
            .projects
            .get(current_project_idx)
            .map(|project| project.name.clone())
            .unwrap_or_else(|| SharedString::from("Project"));

        let toggle_bounds = pane.project_toggle_bounds.clone();
        let outside_bounds = pane.project_toggle_bounds.clone();
        let menu_position = {
            let bounds = pane.project_toggle_bounds.get();
            gpui::point(bounds.right(), bounds.top())
        };

        let menu_bg = if tokens.is_dark {
            rgba(0x202020ff).into()
        } else {
            tokens.card_bg
        };
        let menu_border = if tokens.is_dark {
            rgba(0x3d3d3dff).into()
        } else {
            tokens.card_border
        };
        let accent: Hsla = if tokens.is_dark {
            rgba(0xf47b37ff).into()
        } else {
            rgba(0xc45113ff).into()
        };
        let hover_bg = tokens.pane_header_btn_hover;

        div()
            .relative()
            .child(
                div()
                    .id(("agent-project-selector", pane_id))
                    .debug_selector(|| "agent-project-selector".into())
                    .relative()
                    .h(px(22.0))
                    .px(px(7.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover_bg))
                    .text_size(px(11.5))
                    .text_color(if pane.project_menu_open {
                        tokens.text_primary
                    } else {
                        tokens.text_secondary
                    })
                    .child(
                        canvas(
                            move |bounds, _, _| toggle_bounds.set(bounds),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .child(
                        svg()
                            .path("icons/folder.svg")
                            .size(px(12.0))
                            .flex_shrink_0()
                            .text_color(tokens.text_muted),
                    )
                    .child(div().max_w(px(140.0)).truncate().child(project_name))
                    .child(
                        svg()
                            .path("icons/chevron-down.svg")
                            .size(px(10.0))
                            .flex_shrink_0()
                            .text_color(tokens.text_muted),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(pane) = this.pane_mut(pane_id) {
                                pane.project_menu_open = !pane.project_menu_open;
                                pane.workspace_menu_open = false;
                                pane.branch_picker = None;
                                pane.permission_menu_open = false;
                                pane.plus_menu_open = false;
                                cx.notify();
                            }
                        }),
                    ),
            )
            .when(pane.project_menu_open, |container| {
                let menu = div()
                    .id(("project-popover", pane_id))
                    .debug_selector(|| "agent-project-menu".into())
                    .w(px(260.0))
                    .max_w((window.viewport_size().width - px(24.0)).max(px(0.0)))
                    .p(px(5.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(menu_border)
                    .bg(menu_bg)
                    .shadow_lg()
                    .occlude()
                    .text_size(px(12.0))
                    .text_color(tokens.text_primary)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_mouse_down_out(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        if !outside_bounds.get().contains(&event.position)
                            && let Some(pane) = this.pane_mut(pane_id)
                        {
                            pane.project_menu_open = false;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .px(px(8.0))
                            .pt(px(4.0))
                            .pb(px(4.0))
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(tokens.text_muted)
                            .child("Projects"),
                    )
                    .child(
                        div()
                            .id(("project-switcher-list", pane_id))
                            .max_h(px(200.0))
                            .overflow_y_scrollbar()
                            .flex()
                            .flex_col()
                            .children(self.projects.iter().enumerate().map(|(idx, project)| {
                                let is_active = idx == current_project_idx;
                                let path_display = Self::format_path_display(&project.path);
                                div()
                                    .id(("project-switcher-item", project.id))
                                    .debug_selector(move || format!("project-option-{idx}"))
                                    .px(px(8.0))
                                    .py(px(5.0))
                                    .rounded(px(6.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover_bg))
                                    .child(
                                        svg()
                                            .path("icons/folder.svg")
                                            .size(px(14.0))
                                            .flex_shrink_0()
                                            .text_color(if is_active {
                                                accent
                                            } else {
                                                tokens.text_secondary
                                            }),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_color(if is_active {
                                                        accent
                                                    } else {
                                                        tokens.text_primary
                                                    })
                                                    .child(project.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(10.0))
                                                    .text_color(tokens.text_muted)
                                                    .child(path_display),
                                            ),
                                    )
                                    .when(is_active, |row| {
                                        row.child(
                                            svg()
                                                .path("icons/status-check.svg")
                                                .size(px(13.0))
                                                .flex_shrink_0()
                                                .text_color(accent),
                                        )
                                    })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.switch_agent_project(idx, window, cx);
                                        }),
                                    )
                            })),
                    )
                    .child(div().my(px(4.0)).h(px(1.0)).bg(tokens.border_subtle))
                    .child(
                        div()
                            .id(("project-option-open-folder", pane_id))
                            .debug_selector(|| "project-option-open-folder".into())
                            .px(px(8.0))
                            .py(px(5.0))
                            .min_h(px(28.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover_bg))
                            .child(
                                svg()
                                    .path("icons/folder-plus.svg")
                                    .size(px(14.0))
                                    .flex_shrink_0()
                                    .text_color(tokens.text_secondary),
                            )
                            .child("Open project folder…")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_project_folder_from_switcher(pane_id, window, cx);
                                }),
                            ),
                    );

                container.child(deferred(
                    gpui::anchored()
                        .anchor(gpui::Anchor::BottomRight)
                        .position(menu_position)
                        .offset(gpui::point(px(0.0), px(-6.0)))
                        .child(menu),
                ))
            })
            .into_any_element()
    }
}
