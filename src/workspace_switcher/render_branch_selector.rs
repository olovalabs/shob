impl AppState {
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
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
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
                        }))
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
                                .cursor_text()
                                .on_mouse_down(MouseButton::Left, {
                                    let input = picker.input.clone();
                                    move |_, window, cx| {
                                        if !input.read(cx).focus_handle().is_focused(window) {
                                            input.update(cx, |state, cx| state.focus(window, cx));
                                        }
                                    }
                                })
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
                                            let has_other_worktree =
                                                branch.worktree_path.as_ref().is_some_and(|p| {
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
                                                .when(is_highlighted, |row| row.bg(hover_bg))
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
                                            .when(is_highlighted, |row| row.bg(hover_bg))
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
}
