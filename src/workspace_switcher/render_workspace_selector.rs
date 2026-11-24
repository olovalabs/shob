impl AppState {
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
                                row.cursor_pointer().hover(move |s| s.bg(hover_bg))
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
                                row.cursor_pointer().hover(move |s| s.bg(hover_bg))
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
                                                    row.cursor_pointer()
                                                        .hover(move |s| s.bg(hover_bg))
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
}
