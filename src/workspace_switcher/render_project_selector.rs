impl AppState {
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
        let current_project = self.projects.get(current_project_idx);
        let project_name = current_project
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
                    .map(|trigger| {
                        if let Some(project) = current_project {
                            trigger.child(project_favicon(project, *tokens, px(14.0)))
                        } else {
                            trigger.child(
                                svg()
                                    .path("icons/folder.svg")
                                    .size(px(12.0))
                                    .flex_shrink_0()
                                    .text_color(tokens.text_muted),
                            )
                        }
                    })
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
                    .flex()
                    .flex_col()
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
                            .flex_shrink_0()
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
                            .min_h_0()
                            .max_h(
                                (window.viewport_size().height - px(96.0))
                                    .clamp(px(0.0), px(200.0)),
                            )
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(self.projects.iter().enumerate().filter(|(_, project)| !project.projectless).map(|(idx, project)| {
                                let is_active = idx == current_project_idx;
                                let path_display = Self::format_path_display(&project.path);
                                div()
                                    .id(("project-switcher-item", project.id))
                                    .debug_selector(move || format!("project-option-{idx}"))
                                    .flex_shrink_0()
                                    .px(px(8.0))
                                    .py(px(5.0))
                                    .rounded(px(6.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .cursor_pointer()
                                    .when(is_active, |row| row.bg(hover_bg))
                                    .hover(move |s| s.bg(hover_bg))
                                    .child(project_favicon(project, *tokens, px(18.0)))
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
                            }))
                            .child(
                                div()
                                    .id(("project-option-no-project", pane_id))
                                    .debug_selector(|| "project-option-no-project".into())
                                    .flex_shrink_0()
                                    .px(px(8.0))
                                    .py(px(5.0))
                                    .min_h(px(28.0))
                                    .rounded(px(6.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover_bg))
                                    .child(no_project_icon(px(18.0), tokens.text_secondary))
                                    .child(NO_PROJECT_LABEL)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.close_workspace_popovers(pane_id, cx);
                                            this.choose_new_session_project(None, window, cx);
                                        }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .my(px(4.0))
                            .h(px(1.0))
                            .flex_shrink_0()
                            .bg(tokens.border_subtle),
                    )
                    .child(
                        div()
                            .id(("project-option-open-folder", pane_id))
                            .debug_selector(|| "project-option-open-folder".into())
                            .px(px(8.0))
                            .py(px(5.0))
                            .min_h(px(28.0))
                            .flex_shrink_0()
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
