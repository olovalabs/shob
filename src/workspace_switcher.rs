use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BranchPickerItem {
    Existing(git_workspace::BranchInfo),
    Create(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct GitRequest {
    serial: u64,
    mutation: bool,
}

const BRANCH_ROW_HEIGHT: f32 = 28.0;

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
        .filter(|branch| needle.is_empty() || branch.name.to_lowercase().contains(&needle))
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
            let changed =
                pane.workspace_menu_open || pane.project_menu_open || pane.branch_picker.is_some();
            pane.workspace_menu_open = false;
            pane.project_menu_open = false;
            pane.branch_picker = None;
            if changed {
                cx.notify();
            }
        }
    }

    fn begin_git_request(&mut self, dir: &Path, mutation: bool) -> Option<GitRequest> {
        if self
            .git_requests
            .get(dir)
            .is_some_and(|request| request.mutation || !mutation)
        {
            return None;
        }
        self.git_request_serial = self.git_request_serial.wrapping_add(1);
        let request = GitRequest {
            serial: self.git_request_serial,
            mutation,
        };
        self.git_requests.insert(dir.to_path_buf(), request);
        Some(request)
    }

    fn finish_git_request(&mut self, dir: &Path, request: GitRequest) -> bool {
        if self.git_requests.get(dir) != Some(&request) {
            return false;
        }
        self.git_requests.remove(dir);
        true
    }

    pub(crate) fn schedule_git_refresh(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(request) = self.begin_git_request(&dir, false) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let inspect_dir = dir.clone();
            let snapshot = cx
                .background_executor()
                .spawn(async move { git_workspace::inspect(&inspect_dir) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.finish_git_request(&dir, request) {
                    this.apply_git_snapshot(dir, snapshot, cx);
                }
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
        if self.git_snapshots.get(&dir) == Some(&snapshot) {
            return false;
        }
        let affected: Vec<_> = self
            .projects
            .iter()
            .flat_map(|project| &project.sessions)
            .flat_map(|session| &session.panes)
            .filter(|pane| {
                pane.branch_picker
                    .as_ref()
                    .is_some_and(|picker| picker.directory == dir)
            })
            .map(|pane| pane.id)
            .collect();
        self.git_snapshots.insert(dir, snapshot);
        for pane_id in affected {
            self.update_branch_choices(pane_id, false, cx);
        }
        cx.notify();
        true
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
        pane.workspace_choice = pane.workspace_choice.wrapping_add(1);
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
            pane.chat.shob_session_id = None;
            pane.chat.cwd = Some(target_dir.clone());
            pane.chat.bridge_needs_context = !pane.chat.messages.is_empty();
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
            (Some(main), Some(proj))
                if !git_workspace::paths_equal(&main, &proj) && main.is_dir() =>
            {
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

    pub(super) fn create_and_switch_worktree(
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
        let workspace_choice = pane.workspace_choice;
        let base_branch = self
            .git_snapshots
            .get(&source_dir)
            .and_then(|snap| snap.current_branch.clone());
        let Some(request) = self.begin_git_request(&source_dir, true) else {
            return;
        };

        if let Some(pane) = self.pane_mut(pane_id) {
            pane.workspace_menu_open = false;
            pane.branch_picker = None;
            pane.project_menu_open = false;
        }
        self.git_worktree_creating.insert(pane_id);
        cx.notify();

        let worktrees_root = paths::worktrees_dir();
        cx.spawn_in(window, async move |this, cx| {
            let work_dir = source_dir.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let wt = git_workspace::create_worktree(
                        &work_dir,
                        &worktrees_root,
                        base_branch.as_deref(),
                    )?;
                    let wt_snapshot = git_workspace::inspect(&wt.path);
                    let src_snapshot = git_workspace::inspect(&work_dir);
                    Ok::<_, String>((src_snapshot, wt, wt_snapshot))
                })
                .await;

            let _ = this.update_in(cx, |this, window, cx| {
                this.git_worktree_creating.remove(&pane_id);
                this.finish_git_request(&source_dir, request);
                cx.notify();
                match result {
                    Ok((src_snapshot, wt, wt_snapshot)) => {
                        this.apply_git_snapshot(source_dir, src_snapshot, cx);
                        this.apply_git_snapshot(wt.path.clone(), wt_snapshot, cx);
                        if this
                            .pane_ref(pane_id)
                            .is_some_and(|pane| pane.workspace_choice == workspace_choice)
                        {
                            this.apply_workspace_override(pane_id, Some(wt.path), cx);
                        }
                    }
                    Err(error) => {
                        window.push_notification(
                            Notification::new()
                                .title("Could not create worktree")
                                .message(error)
                                .with_type(NotificationType::Error),
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    }

    fn open_branch_picker(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        if pane.branch_picker.is_some() {
            self.close_branch_picker(pane_id, window, cx);
            return;
        }

        let active_dir = self.agent_working_directory(pane);
        self.schedule_git_refresh(active_dir.clone(), cx);

        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search or create branch..."));
        filter.update(cx, |state, cx| state.focus(window, cx));
        let subscription = cx.subscribe_in(
            &filter,
            window,
            move |this: &mut Self, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.update_branch_choices(pane_id, true, cx);
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
                directory: active_dir,
                items: Rc::from([]),
                sizes: Rc::new(Vec::new()),
                scroll: VirtualListScrollHandle::new(),
                _subscription: subscription,
            });
            self.update_branch_choices(pane_id, true, cx);
        }
    }

    fn update_branch_choices(&mut self, pane_id: usize, reset: bool, cx: &mut Context<Self>) {
        let Some(picker) = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.branch_picker.as_ref())
        else {
            return;
        };
        let query = picker.input.read(cx).value();
        let choices = filtered_branch_items(self.git_snapshots.get(&picker.directory), &query);
        let Some(picker) = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.branch_picker.as_mut())
        else {
            return;
        };
        if reset {
            picker.selected = 0;
            picker.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        } else {
            picker.selected = picker.selected.min(choices.len().saturating_sub(1));
        }
        if &*picker.items != choices.as_slice() {
            picker.sizes = Rc::new(vec![size(px(0.0), px(BRANCH_ROW_HEIGHT)); choices.len()]);
            picker.items = choices.into();
        }
        cx.notify();
    }

    fn close_branch_picker(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
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

    fn move_branch_picker_selection(
        &mut self,
        pane_id: usize,
        delta: isize,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        let Some(picker) = pane.branch_picker.as_ref() else {
            return;
        };
        let items = &picker.items;
        if items.is_empty() {
            return;
        }
        let len = items.len() as isize;
        let next = (picker.selected.min(items.len() - 1) as isize + delta).rem_euclid(len) as usize;
        if let Some(pane) = self.pane_mut(pane_id)
            && let Some(picker) = pane.branch_picker.as_mut()
        {
            picker.selected = next;
            picker
                .scroll
                .scroll_to_item(next, gpui::ScrollStrategy::Top);
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
        let Some(item) = picker.items.get(index).cloned() else {
            return;
        };
        let active_dir = picker.directory.clone();

        self.close_branch_picker(pane_id, window, cx);

        if let BranchPickerItem::Existing(branch) = &item
            && let Some(worktree_path) = branch.worktree_path.as_ref()
            && !git_workspace::paths_equal(worktree_path, &active_dir)
            && worktree_path.is_dir()
        {
            self.select_existing_worktree(pane_id, worktree_path.clone(), window, cx);
            return;
        }
        let Some(request) = self.begin_git_request(&active_dir, true) else {
            window.push_notification(
                Notification::new()
                    .title("Git operation in progress")
                    .message("Wait for the current operation before switching branches.")
                    .with_type(NotificationType::Warning),
                cx,
            );
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let dir = active_dir.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    match item {
                        BranchPickerItem::Existing(branch) => {
                            git_workspace::checkout_branch(&dir, &branch.name, branch.is_remote)?;
                        }
                        BranchPickerItem::Create(name) => {
                            git_workspace::create_and_checkout_branch(&dir, &name)?;
                        }
                    }
                    Ok::<_, String>(git_workspace::inspect(&dir))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.finish_git_request(&active_dir, request) {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        this.apply_git_snapshot(active_dir, snapshot, cx);
                    }
                    Err(error) => window.push_notification(
                        Notification::new()
                            .title("Could not switch branch")
                            .message(error)
                            .with_type(NotificationType::Error),
                        cx,
                    ),
                };
            });
        })
        .detach();
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
        if let Some(pane) = self.focused_pane_id.and_then(|id| self.pane_ref(id)) {
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
                this.open_project_path(path, window, cx);
                if let Some(active_pane) = this.focused_pane_id.and_then(|id| this.pane_ref(id)) {
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
        let active_dir = self.agent_working_directory_ref(pane);
        let snapshot = self.git_snapshots.get(active_dir);

        let left_group = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .min_w_0()
            .child(self.render_workspace_selector(pane, active_dir, snapshot, tokens, window, cx))
            .children(self.render_branch_selector(pane, active_dir, snapshot, tokens, window, cx));

        let show_usage = in_pill_footer
            && pane.chat.worker_view.is_none()
            && pane.chat.usage.is_some_and(|usage| {
                usage.input != 0
                    || usage.output != 0
                    || usage.cache_read != 0
                    || usage.cache_write != 0
                    || usage.cost.is_some_and(|cost| cost > 0.0)
            });

        let mut right_group = div().flex().items_center().gap(px(6.0)).flex_shrink_0();

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
}

include!("workspace_switcher/render_workspace_selector.rs");
include!("workspace_switcher/render_branch_selector.rs");
include!("workspace_switcher/render_project_selector.rs");
