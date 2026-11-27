impl AppState {
    fn navigation_target(&self, backwards: bool) -> Option<(usize, usize, usize)> {
        let len = self.navigation_history.len();
        let mut positions = (0..len).filter_map(|offset| {
            if backwards {
                self.navigation_position.checked_sub(offset + 1)
            } else {
                self.navigation_position
                    .checked_add(offset + 1)
                    .filter(|&position| position < len)
            }
        });
        positions.find_map(|position| {
            let id = self.navigation_history[position];
            self.projects
                .iter()
                .enumerate()
                .find_map(|(project_idx, project)| {
                    project
                        .sessions
                        .iter()
                        .position(|session| session.id == id)
                        .map(|session_idx| (position, project_idx, session_idx))
                })
        })
    }

    fn render_navigation_buttons(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let mut buttons = div().flex().items_center().gap(px(4.0));
        for (backwards, id, icon, title) in [
            (true, "navigation-back", "icons/arrow-left.svg", "Go back"),
            (
                false,
                "navigation-forward",
                "icons/arrow-right.svg",
                "Go forward",
            ),
        ] {
            let target = self.navigation_target(backwards);
            let mut button = div()
                .id(id)
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .child(
                    svg()
                        .path(icon)
                        .size(px(15.0))
                        .text_color(if target.is_some() {
                            tokens.sidebar_icon
                        } else {
                            tokens.sidebar_muted
                        }),
                )
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(title).build(window, cx)
                });
            if let Some((position, project_idx, session_idx)) = target {
                button = button
                    .cursor(CursorStyle::PointingHand)
                    .hover(move |style| style.bg(tokens.sidebar_icon_btn_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.navigation_position = position;
                            this.select_session(project_idx, session_idx, window, cx);
                        }),
                    );
            }
            buttons = buttons.child(button);
        }
        buttons
    }

    fn render_sidebar_actions(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        let picker_mode = self
            .sidebar_project_picker
            .as_ref()
            .map(|picker| picker.mode);
        let scoped = self.sidebar_scope_indices();
        let scope_label = scoped
            .first()
            .and_then(|idx| self.projects.get(*idx))
            .map(|project| project.name.to_string())
            .unwrap_or_else(|| "All projects".to_string());
        let search_query = self.sidebar_search_query(cx);
        let mut sidebar = div()
            .id("sidebar-actions")
            .debug_selector(|| "sidebar-actions".into())
            .relative()
            .flex()
            .flex_col()
            .h_full()
            .w(self.sidebar_width)
            .flex_shrink_0()
            .bg(tokens.sidebar_bg)
            .text_color(tokens.sidebar_fg)
            .child(
                div()
                    .h(px(48.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .px(px(12.0))
                    .gap(px(8.0))
                    .child(sidebar_icon_button(
                        "sidebar-toggle-btn",
                        "icons/panel-left.svg",
                        16.0,
                        &tokens,
                        cx.listener::<MouseDownEvent>(|this, _, window, cx| {
                            this.close_sidebar_search(window, cx);
                            if window.viewport_size().width < COMPACT_WINDOW_WIDTH {
                                this.compact_sidebar_open = false;
                            } else {
                                this.sidebar_visible = false;
                                this.schedule_workspace_save(cx);
                            }
                            cx.notify();
                        }),
                    ))
                    .child(self.render_navigation_buttons(cx)),
            )
            .child(
                div()
                    .id("sidebar-header")
                    .debug_selector(|| "sidebar-header".into())
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(8.0))
                    .pb(px(8.0))
                    .child(
                        div()
                            .id("sidebar-search")
                            .debug_selector(|| "sidebar-search".into())
                            .flex_1()
                            .h(px(32.0))
                            .min_w_0()
                            .flex()
                            .items_center()
                            .px(px(8.0))
                            .rounded(px(8.0))
                            .bg(tokens.card_bg)
                            .border_1()
                            .border_color(tokens.border)
                            .child(
                                svg()
                                    .path("ui_icons/search.svg")
                                    .size(px(14.0))
                                    .text_color(tokens.sidebar_muted),
                            )
                            .child(
                                div()
                                    .id("sidebar-search-field")
                                    .debug_selector(|| "sidebar-search-field".into())
                                    .flex_1()
                                    .min_w_0()
                                    .when_some(self.sidebar_search.as_ref(), |parent, search| {
                                        parent.child(
                                            Input::new(&search.input)
                                                .appearance(false)
                                                .cleanable(false),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        sidebar_icon_action(
                            "sidebar-project-filter",
                            "icons/list-filter.svg",
                            "Filter projects",
                            scoped.len() == 1
                                || picker_mode == Some(SidebarProjectPickerMode::Scope),
                            tokens,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_sidebar_project_picker(
                                    SidebarProjectPickerMode::Scope,
                                    window,
                                    cx,
                                );
                            },
                        )),
                    )
                    .child(
                        sidebar_icon_action(
                            "sidebar-add-project",
                            "icons/folder-plus.svg",
                            "Add project",
                            false,
                            tokens,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.select_folder_and_add_project(window, cx);
                            },
                        )),
                    )
                    .child(
                        sidebar_icon_action(
                            "sidebar-new-thread",
                            "icons/square-pen.svg",
                            "New thread",
                            false,
                            tokens,
                        )
                        .on_click(cx.listener(
                            move |this, event: &gpui::ClickEvent, window, cx| {
                                let shift = event.modifiers().shift;
                                this.handle_sidebar_new_session(shift, window, cx);
                            },
                        )),
                    ),
            );

        if scoped.len() == 1 || !search_query.is_empty() {
            sidebar = sidebar.child(
                div()
                    .id("sidebar-scope-label")
                    .debug_selector(|| "sidebar-scope-label".into())
                    .px(px(12.0))
                    .pb(px(6.0))
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .child(format!(
                        "{scope_label} · {}",
                        if search_query.is_empty() {
                            "Inbox"
                        } else {
                            "Search"
                        }
                    )),
            );
        }

        if let Some(picker) = self.sidebar_project_picker.as_ref()
            && picker.mode == SidebarProjectPickerMode::Scope
        {
            sidebar = sidebar.child(self.render_sidebar_project_picker(picker, cx));
        }
        sidebar = sidebar.child(
            div()
                .id("sidebar-thread-list")
                .debug_selector(|| "sidebar-thread-list".into())
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .px(px(8.0))
                .child(if search_query.is_empty() {
                    self.render_sidebar_inbox(cx)
                } else {
                    self.render_sidebar_search_results(&search_query, cx)
                }),
        );
        sidebar = sidebar.child(self.render_sidebar_footer(cx));
        if self.system_monitor_open {
            sidebar = sidebar.child(deferred(self.render_system_monitor(cx)).priority(101));
        }
        sidebar = sidebar.when(self.sidebar_thread_menu.is_some(), |this| {
            this.child(deferred(self.render_sidebar_thread_menu_overlay(cx)).priority(100))
        });
        sidebar
    }

    fn render_sidebar_project_picker(
        &self,
        picker: &SidebarProjectPicker,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        let query = self.sidebar_project_picker_query(cx);
        let mode = picker.mode;
        let matching = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, project)| sidebar_matches(&query, project.name.as_str()))
            .collect::<Vec<_>>();
        let label = match mode {
            SidebarProjectPickerMode::Scope => "Filter sidebar to",
            SidebarProjectPickerMode::NewSession => "New thread in",
        };
        let mut panel = div()
            .id("sidebar-project-picker")
            .debug_selector(|| "sidebar-project-picker".into())
            .mx(px(8.0))
            .mb(px(8.0))
            .p(px(8.0))
            .rounded(px(10.0))
            .bg(tokens.card_bg)
            .border_1()
            .border_color(tokens.border)
            .child(
                div()
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .child(label),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .mb(px(6.0))
                    .h(px(30.0))
                    .px(px(8.0))
                    .rounded(px(7.0))
                    .bg(tokens.sidebar_bg)
                    .child(Input::new(&picker.input).appearance(false).cleanable(false)),
            );
        if mode == SidebarProjectPickerMode::Scope {
            panel = panel.child(self.sidebar_picker_row(
                "All projects",
                None,
                query.is_empty(),
                mode,
                cx,
            ));
        }
        let no_matches = matching.is_empty();
        for (project_idx, project) in matching {
            panel = panel.child(self.sidebar_picker_row(
                project.name.as_str(),
                Some(project_idx),
                false,
                mode,
                cx,
            ));
        }
        if no_matches {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .child("No matching projects"),
            );
        }
        panel
    }

    fn render_new_session_project_modal(
        &self,
        picker: &SidebarProjectPicker,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        use gpui_component::button::ButtonVariants as _;
        use gpui_component::{Disableable as _, Icon};

        let tokens = self.tokens(cx);
        let query = self.sidebar_project_picker_query(cx);
        let matching: Vec<_> = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, project)| {
                sidebar_matches(&query, project.name.as_str())
                    || sidebar_matches(&query, &project.path.to_string_lossy())
            })
            .collect();
        let selected = picker.selected.min(matching.len().saturating_sub(1));
        let mut list = div()
            .id("new-session-project-list")
            .min_h_0()
            .w_full()
            .max_h(px(320.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.0));
        for (index, (project_idx, project)) in matching.iter().enumerate() {
            let project_idx = *project_idx;
            let highlighted = index == selected;
            list = list.child(
                div()
                    .id(("new-session-project", project_idx))
                    .w_full()
                    .px(px(9.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(10.0))
                    .when(highlighted, |row| row.bg(tokens.sidebar_item_active))
                    .when(!highlighted, |row| row.hover(|row| row.bg(tokens.sidebar_item_hover)))
                    .cursor(CursorStyle::PointingHand)
                    .child(project_favicon(project, tokens, px(24.0)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .truncate()
                                    .child(project.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(tokens.text_secondary)
                                    .truncate()
                                    .child(project.path.to_string_lossy().to_string()),
                            ),
                    )
                    .when(highlighted, |row| {
                        row.child(
                            svg()
                                .path("icons/arrow-right.svg")
                                .size(px(14.0))
                                .mr(px(3.0))
                                .flex_none()
                                .text_color(tokens.text_secondary),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose_new_session_project(Some(project_idx), window, cx)
                    })),
            );
        }
        if matching.is_empty() {
            list = list.child(
                div()
                    .p(px(12.0))
                    .text_sm()
                    .text_color(tokens.text_secondary)
                    .child("No matching projects"),
            );
        }
        let hints = [("↑ ↓", "Navigate"), ("Enter", "Select"), ("Esc", "Close")].map(
            |(keys, label)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(
                        div()
                            .px(px(5.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(tokens.border)
                            .bg(tokens.pane_bg)
                            .text_color(tokens.text_secondary)
                            .child(keys),
                    )
                    .child(label)
            },
        );
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_center()
            .bg(cx.theme().overlay)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_sidebar_project_picker(window, cx)),
            )
            .child(
                div()
                    .id("new-session-project-modal")
                    .debug_selector(|| "new-session-project-modal".into())
                    .w(px(520.0))
                    .max_w(relative(0.9))
                    .max_h(relative(0.9))
                    .occlude()
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.card_bg)
                    .shadow_md()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .gap(px(10.0))
                            .px(px(16.0))
                            .pt(px(14.0))
                            .pb(px(10.0))
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_size(px(16.0))
                                    .child("New thread"),
                            )
                            .child(
                                div()
                                    .h(px(36.0))
                                    .flex()
                                    .items_center()
                                    .px(px(8.0))
                                    .rounded(px(8.0))
                                    .border_1()
                                    .border_color(tokens.border)
                                    .bg(tokens.pane_bg)
                                    .child(
                                        Input::new(&picker.input)
                                            .w_full()
                                            .appearance(false)
                                            .prefix(
                                                svg()
                                                    .path("ui_icons/search.svg")
                                                    .size(px(14.0))
                                                    .text_color(tokens.text_secondary),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .px(px(8.0))
                            .pb(px(8.0))
                            .child(list),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap(px(6.0))
                            .px(px(12.0))
                            .py(px(8.0))
                            .border_t_1()
                            .border_color(tokens.border_subtle)
                            .child(
                                div()
                                    .debug_selector(|| "new-session-bot".into())
                                    .child(
                                        Button::new("new-session-bot")
                                            .small()
                                            .ghost()
                                            .icon(Icon::default().path("ui_icons/settings-gear.svg"))
                                            .label("Bot")
                                            .disabled(true)
                                            .text_color(tokens.text_muted),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(10.0))
                                    .pr(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(tokens.text_secondary)
                                    .children(hints),
                            ),
                    ),
            )
    }

    fn sidebar_picker_row(
        &self,
        label: &str,
        project_idx: Option<usize>,
        active: bool,
        mode: SidebarProjectPickerMode,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        let owned_label = label.to_string();
        let project_id = project_idx
            .and_then(|idx| self.projects.get(idx))
            .map(|project| project.id);
        let renaming = project_id.and_then(|id| {
            self.renaming_project
                .as_ref()
                .filter(|rename| rename.project_id == id)
        });
        let mut row = div()
            .id(ElementId::Name(
                format!("sidebar-project-picker-{owned_label}").into(),
            ))
            .debug_selector(|| "sidebar-project-picker-row".into())
            .min_h(px(30.0))
            .flex()
            .items_center()
            .px(px(8.0))
            .rounded(px(6.0))
            .when(active, |style| {
                style
                    .bg(tokens.sidebar_item_active)
                    .text_color(tokens.sidebar_item_active_fg)
            })
            .hover(|style| style.bg(tokens.sidebar_item_hover))
            .text_sm()
            .cursor(CursorStyle::PointingHand);
        if let Some(rename) = renaming {
            let input = rename.input.clone();
            let id = rename.project_id;
            row = row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&input).cleanable(false)),
                )
                .child(sidebar_icon_button(
                    ("project-rename-save", id),
                    "icons/status-check.svg",
                    12.0,
                    &tokens,
                    cx.listener(move |this, _, _, cx| {
                        let name = input.read(cx).value();
                        this.finish_renaming_project(id, name, cx);
                    }),
                ))
                .child(sidebar_icon_button(
                    ("project-rename-cancel", id),
                    "icons/x.svg",
                    12.0,
                    &tokens,
                    cx.listener(|this, _, _, cx| this.cancel_renaming_project(cx)),
                ));
        } else {
            row = row
                .gap(px(7.0))
                .when_some(project_idx.and_then(|idx| self.projects.get(idx)), |row, project| {
                    row.child(project_favicon(project, tokens, px(16.0)))
                })
                .child(div().flex_1().min_w_0().truncate().child(owned_label));
            if let Some(id) = project_id.filter(|_| mode == SidebarProjectPickerMode::Scope) {
                row = row
                    .child(
                        sidebar_icon_button(
                            ("project-rename", id),
                            "icons/square-pen.svg",
                            12.0,
                            &tokens,
                            cx.listener(move |this, _, window, cx| {
                                this.start_renaming_project(id, window, cx)
                            }),
                        )
                        .tooltip(|window, cx| {
                            gpui_component::tooltip::Tooltip::new("Rename project")
                                .build(window, cx)
                        }),
                    )
                    .child(
                        sidebar_icon_button(
                            ("project-remove", id),
                            "icons/x.svg",
                            12.0,
                            &tokens,
                            cx.listener(move |this, _, window, cx| {
                                if let Some(index) =
                                    this.projects.iter().position(|project| project.id == id)
                                {
                                    this.close_project(index, window, cx);
                                }
                            }),
                        )
                        .tooltip(|window, cx| {
                            gpui_component::tooltip::Tooltip::new("Remove project")
                                .build(window, cx)
                        }),
                    );
            }
            row = row.on_click(cx.listener(move |this, _, window, cx| match mode {
                SidebarProjectPickerMode::Scope => {
                    this.set_sidebar_project_scope(project_idx, window, cx)
                }
                SidebarProjectPickerMode::NewSession => {
                    this.choose_new_session_project(project_idx, window, cx)
                }
            }));
        }
        row
    }

    fn render_sidebar_inbox(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let groups = self.sidebar_thread_groups();
        let active_key = self.sidebar_active_thread_key();
        let now_ms = sidebar_now_ms();
        let mut list = div().w_full().flex().flex_col().gap(px(2.0)).pb(px(8.0));
        if groups.working_count > 0 {
            let expanded = self.settings.sidebar.working_expanded;
            list = list.child(
                div()
                    .id("sidebar-working-toggle")
                    .debug_selector(|| "sidebar-working-toggle".into())
                    .h(px(30.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .cursor(CursorStyle::PointingHand)
                    .hover(|style| style.bg(tokens.sidebar_item_hover))
                    .child("Working")
                    .child(div().flex_1())
                    .child(groups.working_count.to_string())
                    .child(
                        svg()
                            .path(if expanded {
                                "icons/chevron-down.svg"
                            } else {
                                "icons/chevron-right.svg"
                            })
                            .size(px(12.0))
                            .ml(px(6.0)),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings.sidebar.working_expanded =
                            !this.settings.sidebar.working_expanded;
                        this.schedule_workspace_save(cx);
                        cx.notify();
                    })),
            );
            for row in groups
                .working
                .iter()
                .copied()
                .filter(|row| expanded || row.revealed_active)
            {
                list = list.child(self.render_sidebar_thread_card(row, active_key, now_ms, cx));
            }
        }
        for row in groups.pinned.iter().copied() {
            list = list.child(self.render_sidebar_thread_card(row, active_key, now_ms, cx));
        }
        for row in groups.active.iter().copied() {
            list = list.child(self.render_sidebar_thread_card(row, active_key, now_ms, cx));
        }
        if groups.hidden_active > 0 {
            list = list.child(
                div()
                    .id("sidebar-active-show-more")
                    .debug_selector(|| "sidebar-active-show-more".into())
                    .h(px(30.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .cursor(CursorStyle::PointingHand)
                    .child(format!("Show {} more", groups.hidden_active))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_sidebar_active_visible_count(usize::MAX, cx)
                    })),
            );
        }
        if groups.working_count == 0 && groups.pinned.is_empty() && groups.active.is_empty() {
            list = list.child(self.render_sidebar_empty(cx));
        }
        list = list.child(div().flex_1().min_h(px(8.0)));
        if !groups.snoozed.is_empty() || self.settings.sidebar.snoozed_expanded {
            list = list.child(self.render_sidebar_shelf(
                SidebarShelf::Snoozed,
                &groups.snoozed,
                groups.hidden_settled,
                active_key,
                now_ms,
                cx,
            ));
        }
        if !groups.settled.is_empty() || self.settings.sidebar.settled_expanded {
            list = list.child(self.render_sidebar_shelf(
                SidebarShelf::Settled,
                &groups.settled,
                groups.hidden_settled,
                active_key,
                now_ms,
                cx,
            ));
        }
        list
    }

    fn render_sidebar_empty(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        div()
            .id("sidebar-search-empty")
            .debug_selector(|| "sidebar-search-empty".into())
            .py(px(20.0))
            .px(px(10.0))
            .text_sm()
            .text_color(tokens.sidebar_muted)
            .child("No threads yet. Use the pencil button to start one.")
    }

    fn render_sidebar_search_results(&self, query: &str, cx: &mut Context<Self>) -> gpui::Div {
        let rows = self.sidebar_search_thread_refs(query);
        let active_key = self.sidebar_active_thread_key();
        let now_ms = sidebar_now_ms();
        let mut list = div().w_full().flex().flex_col().gap(px(2.0));
        let no_rows = rows.is_empty();
        for row in rows {
            list = list.child(self.render_sidebar_thread_card(row, active_key, now_ms, cx));
        }
        if no_rows {
            list = list.child(
                div()
                    .id("sidebar-search-empty")
                    .debug_selector(|| "sidebar-search-empty".into())
                    .p(px(12.0))
                    .text_xs()
                    .text_color(self.tokens(cx).sidebar_muted)
                    .child("No threads"),
            );
        }
        list
    }

    fn render_sidebar_shelf(
        &self,
        shelf: SidebarShelf,
        rows: &[SidebarThreadRef],
        hidden_settled: usize,
        active_key: Option<(usize, usize)>,
        now_ms: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        let (title, icon, expanded) = match shelf {
            SidebarShelf::Snoozed => (
                "Snoozed",
                "icons/clock.svg",
                self.settings.sidebar.snoozed_expanded,
            ),
            SidebarShelf::Settled => (
                "Settled",
                "icons/status-check.svg",
                self.settings.sidebar.settled_expanded,
            ),
        };
        let mut container = div()
            .id(match shelf {
                SidebarShelf::Snoozed => "sidebar-snoozed-shelf",
                SidebarShelf::Settled => "sidebar-settled-shelf",
            })
            .debug_selector(|| "sidebar-shelf".into())
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("sidebar-shelf-toggle")
                    .h(px(30.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(6.0))
                    .rounded(px(6.0))
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .hover(|style| style.bg(tokens.sidebar_item_hover))
                    .cursor(CursorStyle::PointingHand)
                    .child(
                        svg()
                            .path(icon)
                            .size(px(13.0))
                            .text_color(tokens.sidebar_muted),
                    )
                    .child(title)
                    .child(format!(
                        "({})",
                        rows.len()
                            + if shelf == SidebarShelf::Settled {
                                hidden_settled
                            } else {
                                0
                            }
                    ))
                    .child(div().flex_1())
                    .child(
                        svg()
                            .path(if expanded {
                                "icons/chevron-down.svg"
                            } else {
                                "icons/chevron-right.svg"
                            })
                            .size(px(12.0))
                            .text_color(tokens.sidebar_muted),
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_sidebar_shelf(shelf, cx)),
                    ),
            );
        if expanded {
            for row in rows.iter().copied() {
                container =
                    container.child(self.render_sidebar_thread_slim(row, active_key, now_ms, cx));
            }
            if shelf == SidebarShelf::Settled && hidden_settled > 0 {
                container = container.child(
                    div()
                        .id("sidebar-settled-show-more")
                        .debug_selector(|| "project-show-more".into())
                        .h(px(28.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(tokens.sidebar_muted)
                        .cursor(CursorStyle::PointingHand)
                        .child(format!(
                            "Show {} more",
                            hidden_settled.min(SIDEBAR_SETTLED_PAGE_COUNT)
                        ))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.show_more_settled_threads(cx)),
                        ),
                );
            }
        }
        container
    }
}
