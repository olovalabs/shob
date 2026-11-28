impl AppState {
    fn render_preview_tab(
        &self,
        pane_id: usize,
        path: &str,
        return_to_changes: bool,
        deleted: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let tokens = self.tokens(cx);
        let editor_bg = cx.theme().editor_background();
        let path_tip: SharedString = if deleted {
            format!("{path} (last committed version)").into()
        } else {
            path.to_owned().into()
        };
        div()
            .h(px(36.0))
            .flex_shrink_0()
            .border_b_1()
            .border_color(tokens.border_subtle)
            .bg(tokens.pane_header_bg)
            .flex()
            .child(
                div()
                    .id(("preview-tab", pane_id))
                    .h_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .pl(px(12.0))
                    .pr(px(6.0))
                    .border_r_1()
                    .border_color(tokens.border_subtle)
                    .bg(editor_bg)
                    .child(
                        svg()
                            .path(file_icons::icon_for_path(path))
                            .size(px(15.0))
                            .flex_shrink_0()
                            .text_color(tokens.text_secondary),
                    )
                    .child(
                        div()
                            .id(("preview-filename", pane_id))
                            .min_w_0()
                            .max_w(px(250.0))
                            .truncate()
                            .text_size(px(13.0))
                            .text_color(if deleted {
                                tokens.text_muted
                            } else {
                                tokens.text_primary
                            })
                            .when(deleted, |label| label.line_through())
                            .tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(path_tip.clone())
                                    .build(window, cx)
                            })
                            .child(file_name(path).to_owned()),
                    )
                    .child(
                        div()
                            .id(("close-preview-tab", pane_id))
                            .debug_selector(|| "close-preview-tab".to_string())
                            .size(px(22.0))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(tokens.pane_header_btn_hover))
                            .tooltip(move |window, cx| {
                                let label = if return_to_changes {
                                    "Back to changes"
                                } else {
                                    "Close file"
                                };
                                gpui_component::tooltip::Tooltip::new(label).build(window, cx)
                            })
                            .child(
                                svg()
                                    .path("icons/x.svg")
                                    .size(px(12.0))
                                    .text_color(tokens.text_secondary),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    if return_to_changes {
                                        if let Some(view) = this
                                            .pane_mut(pane_id)
                                            .and_then(|pane| pane.diff_preview.as_mut())
                                        {
                                            view.file_request += 1;
                                            view.file_view = None;
                                            this.focus_pane(pane_id, window, cx);
                                        }
                                    } else {
                                        this.close_pane(pane_id, window, cx);
                                    }
                                }),
                            ),
                    ),
            )
    }

    fn render_changed_file(&self, file: &ReadOnlyFile, cx: &mut Context<Self>) -> AnyElement {
        let tokens = self.tokens(cx);
        let editor_bg = cx.theme().editor_background();
        let body = match &file.state {
            ReadOnlyFileState::Loading => div()
                .p(px(16.0))
                .text_color(tokens.text_secondary)
                .child("Loading file…")
                .into_any_element(),
            ReadOnlyFileState::Error(error) => div()
                .p(px(16.0))
                .text_color(tokens.text_secondary)
                .child(error.clone())
                .into_any_element(),
            ReadOnlyFileState::Ready { input, .. } => div()
                .flex_1()
                .min_h_0()
                .size_full()
                .child(
                    Input::new(input)
                        .appearance(false)
                        .bordered(false)
                        .font_family(MONO_FONT)
                        .text_size(px(13.0))
                        .line_height(px(21.0))
                        .px(px(0.0))
                        .py(px(4.0))
                        .size_full(),
                )
                .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(editor_bg)
            .child(body)
            .into_any_element()
    }

    fn render_inline_patch(
        &self,
        pane_id: usize,
        file: &project_diff::DiffFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use project_diff::DiffLineKind;

        let tokens = self.tokens(cx);
        let theme = cx.theme().highlight_theme.clone();
        let foreground = theme.style.editor_foreground.unwrap_or(tokens.text_primary);
        let editor_bg = cx.theme().editor_background();
        let added = if tokens.is_dark {
            rgba(0x4caf792e)
        } else {
            rgba(0x248c5630)
        };
        let removed = if tokens.is_dark {
            rgba(0xe06c7633)
        } else {
            rgba(0xc94a5730)
        };
        let key = format!(
            "inline-patch-{pane_id}-{}-{:p}-{:p}",
            file.path,
            file.patch.as_ptr(),
            Arc::as_ptr(&theme)
        );
        let state = window.use_keyed_state(key, cx, |_, cx| {
            let path = file.path.clone();
            let patch = file.patch.clone();
            let theme_for_worker = theme.clone();
            let patch_for_worker = patch.clone();
            cx.spawn(async move |state, cx| {
                let diff = cx
                    .background_executor()
                    .spawn(async move {
                        project_diff::InlineDiff::from_patch(
                            &path,
                            &patch_for_worker,
                            &theme_for_worker,
                        )
                    })
                    .await;
                let _ = state.update(cx, |state: &mut InlinePatchState, cx| {
                    state.diff = Some(diff);
                    cx.notify();
                });
            })
            .detach();
            InlinePatchState {
                _patch: patch,
                _theme: theme,
                diff: None,
            }
        });
        let diff = state.read(cx).diff.as_ref();
        let mut code = div().flex().flex_col().flex_shrink_0().min_w(relative(1.0));
        if let Some(diff) = diff {
            for (index, hunk) in diff.hunks.iter().enumerate() {
                if index > 0 {
                    code = code.child(div().h(px(8.0)).bg(tokens.pane_header_bg));
                }
                for line in hunk {
                    code = code.child(
                        div()
                            .debug_selector({
                                let kind = line.kind;
                                move || match kind {
                                    DiffLineKind::Context => "inline-diff-context".into(),
                                    DiffLineKind::Added => "inline-diff-added".into(),
                                    DiffLineKind::Removed => "inline-diff-removed".into(),
                                }
                            })
                            .h(px(20.0))
                            .px(px(18.0))
                            .when(line.kind == DiffLineKind::Added, |row| row.bg(added))
                            .when(line.kind == DiffLineKind::Removed, |row| row.bg(removed))
                            .child(
                                StyledText::new(line.text.clone())
                                    .with_highlights(line.highlights.iter().cloned()),
                            ),
                    );
                }
            }
        }
        div()
            .id((
                SharedString::from(format!("inline-diff-{}", file.path)),
                pane_id,
            ))
            .debug_selector(|| "inline-diff".to_owned())
            .w_full()
            .border_t_1()
            .border_color(tokens.border_subtle)
            .bg(editor_bg)
            .child(
                div()
                    .id("inline-code")
                    .w_full()
                    .overflow_x_scroll()
                    .font_family(MONO_FONT)
                    .text_size(px(13.0))
                    .line_height(px(20.0))
                    .text_color(foreground)
                    .whitespace_nowrap()
                    .child(code),
            )
            .when(
                diff.is_none() || diff.is_some_and(|diff| diff.truncated || diff.hunks.is_empty()),
                |patch| {
                    patch.child(
                        div()
                            .px(px(12.0))
                            .py(px(6.0))
                            .text_size(px(11.0))
                            .text_color(tokens.text_muted)
                            .child(match diff {
                                None => "Loading diff…",
                                Some(diff) if diff.truncated => "Diff preview truncated.",
                                _ if file.additions.is_none() && file.deletions.is_none() => {
                                    "Binary file"
                                }
                                _ => "No text changes",
                            }),
                    )
                },
            )
    }

    fn render_project_diff(
        &self,
        pane_id: usize,
        view: &DiffPreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(file) = &view.file_view {
            return self.render_changed_file(file, cx);
        }
        let tokens = self.tokens(cx);
        let mut content = div()
            .id(("project-diff-content", pane_id))
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .flex()
            .flex_col()
            .p(px(16.0))
            .gap(px(8.0));
        let mut summary = "Loading project changes…".to_string();
        let mut totals = None;
        let added_color = if tokens.is_dark {
            rgba(0x3fb950ff)
        } else {
            rgba(0x1a7f37ff)
        };
        let removed_color = if tokens.is_dark {
            rgba(0xf85149ff)
        } else {
            rgba(0xcf222eff)
        };
        match &view.result {
            Some(Err(error)) => {
                summary = "Could not load changes".to_owned();
                content =
                    content.child(div().text_color(tokens.text_secondary).child(error.clone()));
            }
            Some(Ok(diff)) => {
                let additions: usize = diff.files.iter().filter_map(|f| f.additions).sum();
                let deletions: usize = diff.files.iter().filter_map(|f| f.deletions).sum();
                summary = format!("{} files changed", diff.files.len());
                totals = Some((additions, deletions));
                if diff.files.is_empty() {
                    content = content.child(
                        div()
                            .text_color(tokens.text_secondary)
                            .child("No project changes."),
                    );
                }
                for (group, title, collapsed) in [
                    (
                        project_diff::FileGroup::Tracked,
                        "Tracked",
                        view.tracked_collapsed,
                    ),
                    (
                        project_diff::FileGroup::Untracked,
                        "Untracked",
                        view.untracked_collapsed,
                    ),
                ] {
                    let file_count = diff.files.iter().filter(|file| file.group == group).count();
                    if file_count == 0 {
                        continue;
                    }
                    content = content.child(
                        div()
                            .id((title, pane_id))
                            .debug_selector(move || format!("project-diff-section-{title}"))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(6.0))
                            .py(px(6.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .hover(|s| s.bg(tokens.card_hover))
                            .text_color(tokens.text_secondary)
                            .text_size(px(12.0))
                            .child(if collapsed { "▸" } else { "▾" })
                            .child(format!("{title} ({file_count})"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(view) = this
                                        .pane_mut(pane_id)
                                        .and_then(|pane| pane.diff_preview.as_mut())
                                    {
                                        match group {
                                            project_diff::FileGroup::Tracked => {
                                                view.tracked_collapsed = !view.tracked_collapsed
                                            }
                                            project_diff::FileGroup::Untracked => {
                                                view.untracked_collapsed = !view.untracked_collapsed
                                            }
                                        }
                                        cx.notify();
                                    }
                                }),
                            ),
                    );
                    if collapsed {
                        continue;
                    }
                    for (index, file) in diff
                        .files
                        .iter()
                        .enumerate()
                        .filter(|(_, file)| file.group == group)
                    {
                        let path = file.path.clone();
                        let toggle_path = path.clone();
                        let expanded = view.expanded.contains(&path);
                        let counts = (file.additions, file.deletions);
                        let mut card = div()
                            .flex()
                            .flex_col()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(tokens.border_subtle)
                            .child(
                                div()
                                    .id(("project-diff-file", pane_id * 100000 + index))
                                    .debug_selector(move || format!("project-diff-file-{index}"))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(8.0))
                                    .px(px(10.0))
                                    .py(px(7.0))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(tokens.card_hover))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(6.0))
                                            .min_w_0()
                                            .text_color(tokens.text_primary)
                                            .text_size(px(12.0))
                                            .child(
                                                div()
                                                    .id((
                                                        "toggle-diff-file",
                                                        pane_id * 100000 + index,
                                                    ))
                                                    .px(px(3.0))
                                                    .cursor_pointer()
                                                    .child(if expanded { "▾" } else { "▸" })
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(move |this, _, _, cx| {
                                                            cx.stop_propagation();
                                                            if let Some(view) = this
                                                                .pane_mut(pane_id)
                                                                .and_then(|pane| {
                                                                    pane.diff_preview.as_mut()
                                                                })
                                                            {
                                                                if !view
                                                                    .expanded
                                                                    .remove(&toggle_path)
                                                                {
                                                                    view.expanded.insert(
                                                                        toggle_path.clone(),
                                                                    );
                                                                }
                                                                cx.notify();
                                                            }
                                                        }),
                                                    ),
                                            )
                                            .child(
                                                svg()
                                                    .path(file_icons::icon_for_path(&path))
                                                    .size(px(16.0))
                                                    .text_color(tokens.text_secondary),
                                            )
                                            .child(div().min_w_0().child(path.clone())),
                                    )
                                    .child(diff_counts(
                                        counts,
                                        added_color,
                                        removed_color,
                                        tokens.text_secondary,
                                    ))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.open_changed_file(
                                                pane_id,
                                                path.clone(),
                                                window,
                                                cx,
                                            );
                                        }),
                                    ),
                            );
                        if expanded {
                            card = card.child(self.render_inline_patch(pane_id, file, window, cx));
                        }
                        content = content.child(card);
                    }
                }
            }
            None => {}
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(16.0))
                    .py(px(10.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .text_size(px(12.0))
                            .child(div().text_color(tokens.text_primary).child(summary))
                            .when_some(totals, |row, (additions, deletions)| {
                                row.child(diff_counts(
                                    (Some(additions), Some(deletions)),
                                    added_color,
                                    removed_color,
                                    tokens.text_secondary,
                                ))
                            }),
                    )
                    .child(
                        div()
                            .id(("refresh-project-diff", pane_id))
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .hover(|s| s.bg(tokens.card_hover))
                            .text_color(tokens.text_secondary)
                            .text_size(px(11.0))
                            .child("Refresh")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.refresh_project_diff(pane_id, cx);
                                }),
                            ),
                    ),
            )
            .child(content)
            .into_any_element()
    }

    fn toggle_maximize_pane(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(&(p_idx, s_idx, _)) = self.pane_index.get(&pane_id) else {
            return;
        };

        if let Some(session) = self
            .projects
            .get_mut(p_idx)
            .and_then(|p| p.sessions.get_mut(s_idx))
        {
            if session.maximized_pane_id == Some(pane_id) {
                session.maximized_pane_id = None;
            } else {
                session.maximized_pane_id = Some(pane_id);
            }
            self.schedule_workspace_save(cx);
            cx.notify();
        }
    }

    /// Close one pane inside its session's screen. Closing the last pane also
    /// removes the session (screen) itself; an emptied project spawns a fresh
    /// session, like before.
    fn close_pane(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let panel_owner = self
            .pane_ref(pane_id)
            .filter(|pane| pane.tool_panel)
            .and_then(|pane| pane.preview_owner);
        if let Some(view) = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.files_surface.as_ref())
            .map(|(view, _)| view.clone())
            && !view.update(cx, |view, cx| view.prepare_close(cx))
        {
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.files_active = true;
                pane.display_title = "Files".into();
            }
            if self.settings.agent_layout
                && let Some(owner) = panel_owner
            {
                self.select_tool_panel_tab(owner, pane_id, window, cx);
            }
            cx.notify();
            return;
        }
        let Some(&(p_idx, s_idx, _)) = self.pane_index.get(&pane_id) else {
            return;
        };
        let closing_selected_tab = panel_owner.is_some_and(|owner| {
            self.pane_ref(owner)
                .and_then(|pane| self.active_tool_panel(pane))
                .is_some_and(|pane| pane.id == pane_id)
        });

        let mut session_removed = false;
        let mut needs_new_session = false;
        if let Some(project) = self.projects.get_mut(p_idx) {
            if let Some(session) = project.sessions.get_mut(s_idx) {
                if session.maximized_pane_id == Some(pane_id) {
                    session.maximized_pane_id = None;
                }
                if let Some(closed_pane) = session.panes.iter().find(|pane| pane.id == pane_id) {
                    status::forget_pid(closed_pane.pid);
                }
                let tree = &mut session.pane_tree;
                *tree = tree.take().and_then(|tree| tree.remove(pane_id));
                session.panes.retain_mut(|pane| {
                    if pane.id == pane_id {
                        return false;
                    }
                    if pane.tool_panel && pane.preview_owner == Some(pane_id) {
                        if pane.is_browser() {
                            *tree = tree.take().and_then(|tree| tree.remove(pane.id));
                            return false;
                        }
                        pane.one_mode = false;
                        pane.tool_panel = false;
                        pane.preview_owner = None;
                    }
                    true
                });
                if session.pane_tree.is_none() || session.panes.is_empty() {
                    session_removed = true;
                }
            }
            if session_removed {
                project.sessions.retain(|s| !s.panes.is_empty());
                if project.sessions.is_empty() {
                    needs_new_session = true;
                } else {
                    project.active_session_idx =
                        project.active_session_idx.min(project.sessions.len() - 1);
                }
            } else if panel_owner.is_none()
                && let Some(session) = project.sessions.get(s_idx)
            {
                // Focus the surviving first pane of this screen.
                let next_leaf = session
                    .pane_tree
                    .as_ref()
                    .map(|t| t.first_leaf())
                    .and_then(|id| session.panes.iter().find(|p| p.id == id));
                if let Some(pane) = next_leaf {
                    let pid = pane.id;
                    self.focus_pane(pid, window, cx);
                }
            }
        }

        self.rebuild_session_index();
        if let Some(owner) = panel_owner {
            let next = self.tool_panel_panes(owner).next().map(|pane| pane.id);
            if let Some(pane) = self.pane_mut(owner)
                && (closing_selected_tab || pane.tool_panel_tab == Some(pane_id))
            {
                pane.tool_panel_tab = next;
            }
            if self.settings.agent_layout && closing_selected_tab && next.is_none() {
                self.show_tool_panel_chooser(owner, window, cx);
            } else if self.focused_pane_id == Some(pane_id) {
                if self.settings.agent_layout
                    && let Some(next) = next
                {
                    self.select_tool_panel_tab(owner, next, window, cx);
                } else {
                    self.focus_pane(owner, window, cx);
                }
            }
        }

        if needs_new_session {
            self.create_new_session_in_project(p_idx, window, cx);
        } else {
            self.schedule_workspace_save(cx);
            cx.notify();
        }
    }

    fn focus_pane(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut pane_id = pane_id;
        if !self.settings.agent_layout
            && let Some(pane) = self.pane_ref(pane_id)
        {
            if pane.tool_panel {
                pane_id = pane.preview_owner.unwrap_or(pane_id);
            }
            if self
                .pane_ref(pane_id)
                .is_some_and(|pane| pane.one_mode || pane.tool_panel)
                && let Some(&(project, session, _)) = self.pane_index.get(&pane_id)
            {
                let screen = &self.projects[project].sessions[session];
                let regular_focus = self.pane_ref(pane_id).and_then(|pane| pane.regular_focus);
                let regular = || {
                    screen
                        .panes
                        .iter()
                        .filter(|pane| !pane.one_mode && !pane.tool_panel)
                };
                let Some(target) = regular()
                    .find(|pane| Some(pane.id) == regular_focus)
                    .or_else(|| regular().next())
                else {
                    self.active_project_idx = project;
                    self.projects[project].active_session_idx = session;
                    self.focused_pane_id = None;
                    self.active_view = ActiveView::Workspace;
                    cx.notify();
                    return;
                };
                pane_id = target.id;
            }
        }
        self.active_view = ActiveView::Workspace;
        self.focused_pane_id = Some(pane_id);
        if let Some(&(p_idx, s_idx, _)) = self.pane_index.get(&pane_id) {
            self.active_project_idx = p_idx;
            if let Some(p) = self.projects.get_mut(p_idx) {
                p.active_session_idx = s_idx;
                if let Some(session) = p.sessions.get_mut(s_idx)
                    && let Some(pane) = session.panes.iter_mut().find(|p| p.id == pane_id)
                {
                    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
                    if let Some(browser) = Self::mount_browser(pane, window, cx) {
                        browser.view.update(cx, |view, cx| view.focus_address(window, cx));
                        self.dirty = true;
                        cx.notify();
                        return;
                    }
                    if pane.chat.auth.is_some() || pane.files_active {
                        return;
                    }
                    if let Some(ReadOnlyFile {
                        state: ReadOnlyFileState::Ready { input, .. },
                        ..
                    }) = pane
                        .diff_preview
                        .as_ref()
                        .and_then(|view| view.file_view.as_ref())
                    {
                        input.update(cx, |input, cx| input.focus(window, cx));
                        cx.notify();
                        return;
                    }
                    let should_show = pane.should_show_bottom_input(cx);
                    pane.terminal.update(cx, |term, _| {
                        term.set_read_only(should_show);
                    });
                    if should_show {
                        if let Some(input_state) = &pane.input_state {
                            input_state.update(cx, |state, cx| {
                                state.focus(window, cx);
                            });
                        } else {
                            pane.terminal
                                .read(cx)
                                .focus_handle()
                                .clone()
                                .focus(window, cx);
                        }
                    } else {
                        pane.terminal
                            .read(cx)
                            .focus_handle()
                            .clone()
                            .focus(window, cx);
                    }
                }
            }
        }
        if let Some(pane) = self.pane_ref(pane_id)
            && pane.chat.active
        {
            let dir = self.agent_working_directory(pane);
            if !self.git_snapshots.contains_key(&dir) {
                self.schedule_git_refresh(dir, cx);
            }
        }
        self.dirty = true;
        cx.notify();
    }

    fn focus_pane_input(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .pane_ref(pane_id)
            .is_some_and(|p| p.chat.auth.is_some())
        {
            return;
        }
        self.active_view = ActiveView::Workspace;
        self.focused_pane_id = Some(pane_id);
        if let Some(&(p_idx, s_idx, pane_idx)) = self.pane_index.get(&pane_id) {
            self.active_project_idx = p_idx;
            if let Some(pane) = self
                .projects
                .get_mut(p_idx)
                .and_then(|p| p.sessions.get_mut(s_idx))
                .and_then(|s| s.panes.get_mut(pane_idx))
                && let Some(input_state) = pane.input_state.as_ref()
            {
                input_state.update(cx, |state, cx| {
                    state.focus(window, cx);
                });
            }
        }
        cx.notify();
    }

    fn pane_ref(&self, pane_id: usize) -> Option<&TerminalPane> {
        let &(p_idx, s_idx, pane_idx) = self.pane_index.get(&pane_id)?;
        self.projects
            .get(p_idx)?
            .sessions
            .get(s_idx)?
            .panes
            .get(pane_idx)
    }

    fn pane_mut(&mut self, pane_id: usize) -> Option<&mut TerminalPane> {
        let &(p_idx, s_idx, pane_idx) = self.pane_index.get(&pane_id)?;
        self.projects
            .get_mut(p_idx)?
            .sessions
            .get_mut(s_idx)?
            .panes
            .get_mut(pane_idx)
    }

    /// Shell history plus in-session commands, oldest first.
    fn pane_history_entries(&self, pane: &TerminalPane) -> Vec<String> {
        shell_history::merge(&pane.shell_history, &pane.command_history)
    }

    /// Snapshot of everything suggestion code needs from a pane's input.
    fn input_context(&self, pane_id: usize, cx: &App) -> Option<PaneInputContext> {
        let pane = self.pane_ref(pane_id)?;
        let input = pane.input_state.clone()?;
        let value = input.read(cx).value().to_string();
        let cursor = input.read(cx).cursor();
        let cwd = self
            .pane_cwd_path(pane)
            .unwrap_or_else(|| PathBuf::from("."));
        Some((value, cursor, cwd, self.pane_history_entries(pane), input))
    }

    /// Create a pane's input editor and subscribe to its changes.
    fn create_pane_input(
        &mut self,
        pane_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let (input, subscription) = build_pane_input(pane_id, window, cx);
        if self.pane_ref(pane_id).is_some_and(|pane| pane.chat.active) {
            input.update(cx, |state, cx| {
                state.clear_highlighter(cx);
                state.set_placeholder("Message the agent…", window, cx);
            });
        }
        let restore = self
            .pane_mut(pane_id)
            .and_then(|pane| pane.restore_draft.take());
        if let Some(text) = restore.filter(|text| !text.is_empty()) {
            set_input_text(&input, text, window, cx);
        }
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.input_state = Some(input.clone());
            pane.input_subscription = Some(subscription);
        }
        input
    }
}
