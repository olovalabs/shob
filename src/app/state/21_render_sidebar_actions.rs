impl AppState {
    fn sidebar_tokens(&self, cx: &App) -> ThemeTokens {
        let mut tokens = self.tokens(cx);
        if self.palette_scheme != PaletteScheme::Shob {
            return tokens;
        }
        let dark = tokens.is_dark;
        let mode = if dark {
            gpui_component::ThemeMode::Dark
        } else {
            gpui_component::ThemeMode::Light
        };
        let defaults = ThemeTokens::from_mode_with_scheme(mode, self.palette_scheme);
        let match_default = |current: &mut Hsla, original: Hsla, dark_color, light_color| {
            if terminal::hsla_to_rgb(*current) == terminal::hsla_to_rgb(original)
                && (current.a - original.a).abs() < 1.0 / 255.0
            {
                *current = rgba(if dark { dark_color } else { light_color }).into();
            }
        };
        macro_rules! sidebar_color {
            ($field:ident, $dark:expr, $light:expr) => {
                match_default(&mut tokens.$field, defaults.$field, $dark, $light)
            };
        }
        sidebar_color!(sidebar_bg, 0x111111ff, 0xfafafaff);
        sidebar_color!(sidebar_fg, 0xf5f5f5ff, 0x18181bff);
        sidebar_color!(sidebar_muted, 0xa3a3a3ff, 0x71717aff);
        sidebar_color!(sidebar_border, 0xffffff14, 0xe4e4e7ff);
        sidebar_color!(sidebar_item_hover, 0xffffff0a, 0xfdfdfdff);
        sidebar_color!(sidebar_item_active, 0xffffff08, 0xffffffff);
        sidebar_color!(sidebar_item_active_fg, 0xf5f5f5ff, 0x18181bff);
        sidebar_color!(sidebar_icon, 0xa3a3a3ff, 0x71717aff);
        sidebar_color!(sidebar_icon_hover, 0xf5f5f5ff, 0x18181bff);
        sidebar_color!(sidebar_icon_btn_hover, 0xffffff0a, 0xfdfdfdff);
        tokens
    }

    fn render_sidebar_toggle(
        &self,
        open: bool,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = if open { "sidebar-toggle-btn" } else { "terminal-sidebar-toggle-btn" };
        let label = if cfg!(target_os = "macos") {
            "Toggle main sidebar (⌘B)"
        } else {
            "Toggle main sidebar (Ctrl+B)"
        };
        let button = sidebar_icon_action(id, "icons/panel-left.svg", "Toggle main sidebar", false, tokens)
            .aria_expanded(open)
            .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx)));
        self.sidebar_tooltip(button, id, label, SidebarTooltipSide::Bottom, tokens, cx)
    }

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

    fn render_navigation_buttons(&self, tokens: ThemeTokens, cx: &mut Context<Self>) -> gpui::Div {
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
            let mut button = sidebar_icon_action(id, icon, title, false, tokens)
                .when(target.is_none(), |button| button.opacity(0.45));
            if let Some((position, project_idx, session_idx)) = target {
                button = button.on_click(cx.listener(move |this, _, window, cx| {
                    this.navigation_position = position;
                    this.select_session(project_idx, session_idx, window, cx);
                }));
            }
            buttons = buttons.child(self.sidebar_tooltip(button, id, title, SidebarTooltipSide::Bottom, tokens, cx));
        }
        buttons
    }

    fn render_sidebar_actions(
        &self,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let picker_mode = self
            .sidebar_project_picker
            .as_ref()
            .map(|picker| picker.mode);
        let scoped = self.sidebar_scope_indices();
        let search_query = self.sidebar_search_query(cx);
        self.sidebar_navigation_begin(!search_query.is_empty());
        if !search_query.is_empty() {
            self.reset_sidebar_motion();
        }
        let new_thread_tooltip = if cfg!(target_os = "macos") {
            "New thread (⌘N)"
        } else {
            "New thread (Ctrl+N)"
        };
        let mut sidebar = div()
            .id("sidebar-actions")
            .debug_selector(|| "sidebar-actions".into())
            .relative()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .flex_shrink_0()
            .bg(tokens.sidebar_bg)
            .text_color(tokens.sidebar_fg)
            .child(
                div()
                    .h(SIDEBAR_CHROME_HEIGHT)
                    .w_full()
                    .flex()
                    .items_center()
                    .pl(px(9.0))
                    .pr(px(12.0))
                    .gap(px(4.0))
                    .child(self.render_sidebar_toggle(true, tokens, cx))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Shob"),
                    )
                    .child(self.render_navigation_buttons(tokens, cx)),
            )
            .child(
                div()
                    .id("sidebar-header")
                    .debug_selector(|| "sidebar-header".into())
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(8.0))
                    .pb(px(8.0))
                    .child(
                        div()
                            .id("sidebar-search")
                            .role(gpui::Role::Group)
                            .aria_label("Search threads")
                            .debug_selector(|| "sidebar-search".into())
                            .flex_1()
                            .h(px(32.0))
                            .min_w_0()
                            .gap(px(4.0))
                            .flex()
                            .items_center()
                            .px(px(8.0))
                            .rounded(px(6.0))
                            .hover(move |style| style.bg(tokens.sidebar_item_hover))
                            .child(
                                svg()
                                    .path("ui_icons/search.svg")
                                    .size(px(16.0))
                                    .flex_shrink_0()
                                    .text_color(tokens.sidebar_icon),
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
                                                .cleanable(true),
                                        )
                                    }),
                            ),
                    )
                    .when(!self.projects.is_empty(), |header| {
                        let button = sidebar_icon_action(
                                "sidebar-project-filter",
                                "icons/folder.svg",
                                "Projects",
                                picker_mode == Some(SidebarProjectPickerMode::Scope),
                                tokens,
                            )
                            .when(!scoped.is_empty(), |button| {
                                button.relative().child(
                                    div()
                                        .absolute()
                                        .size(px(24.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .bg(tokens.sidebar_bg)
                                        .children(scoped.iter().take(2).map(|&index| {
                                            project_favicon(
                                                &self.projects[index],
                                                tokens,
                                                px(if scoped.len() == 1 { 16.0 } else { 11.0 }),
                                            )
                                        })),
                                )
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.sidebar_project_picker.as_ref().is_some_and(|picker| {
                                    picker.mode == SidebarProjectPickerMode::Scope
                                }) {
                                    this.close_sidebar_project_picker(window, cx);
                                } else {
                                    this.open_sidebar_project_picker(
                                        SidebarProjectPickerMode::Scope,
                                        window,
                                        cx,
                                    );
                                }
                            }));
                        let label: SharedString = if scoped.is_empty() {
                            "Projects".into()
                        } else {
                            format!("Projects: {}", scoped.iter().map(|&index| self.projects[index].name.as_str()).collect::<Vec<_>>().join(", ")).into()
                        };
                        header.child(self.sidebar_tooltip(button, "sidebar-project-filter", label, SidebarTooltipSide::Top, tokens, cx))
                    })
                    .child(
                        self.sidebar_tooltip(sidebar_icon_action(
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
                            "sidebar-new-thread",
                            new_thread_tooltip,
                            SidebarTooltipSide::Top,
                            tokens,
                            cx,
                        ),
                    ),
            );

        if let Some(picker) = self.sidebar_project_picker.as_ref()
            && picker.mode == SidebarProjectPickerMode::Scope
        {
            sidebar = sidebar.child(self.render_sidebar_project_picker(picker, tokens, cx));
        }
        sidebar = sidebar.child(
            div()
                .id("sidebar-thread-list")
                .debug_selector(|| "sidebar-thread-list".into())
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .flex_col()
                .px(px(8.0))
                .when(search_query.is_empty(), |list| {
                    list.overflow_y_scroll().track_scroll(&self.sidebar_inbox_scroll)
                        .child(self.render_sidebar_inbox(tokens, cx))
                })
                .when(!search_query.is_empty(), |list| {
                    list.child(self.render_sidebar_search_results(&search_query, tokens, cx))
                }),
        );
        sidebar = sidebar.child(self.render_sidebar_footer(tokens, cx));
        if self.system_monitor_open {
            sidebar = sidebar.child(deferred(self.render_system_monitor(cx)).priority(101));
        }
        sidebar = sidebar.when(self.sidebar_thread_menu.is_some(), |this| {
            this.child(deferred(self.render_sidebar_thread_menu_overlay(tokens, cx)).priority(100))
        });
        sidebar = sidebar.when(self.sidebar_folder_menu.is_some(), |this| {
            this.child(deferred(self.render_sidebar_folder_menu(tokens, cx)).priority(100))
        });
        self.sidebar_navigation_finish();
        sidebar
    }

    fn render_sidebar_project_picker(
        &self,
        picker: &SidebarProjectPicker,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let query = self.sidebar_project_picker_query(cx);
        let mode = picker.mode;
        let scoped = self.sidebar_scope_indices();
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
            .p(px(4.0))
            .max_h(px(304.0))
            .overflow_y_scroll()
            .rounded(px(8.0))
            .bg(tokens.sidebar_bg)
            .border_1()
            .border_color(tokens.sidebar_border)
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
                scoped.is_empty(),
                mode,
                cx,
            ));
        }
        let no_matches = matching.is_empty();
        for (project_idx, project) in matching {
            panel = panel.child(self.sidebar_picker_row(
                project.name.as_str(),
                Some(project_idx),
                scoped.contains(&project_idx),
                mode,
                cx,
            ));
        }
        if no_matches {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .child("No matching projects."),
            );
        }
        panel = panel.child(
            div()
                .id("sidebar-add-project")
                .tab_index(0)
                .h(px(32.0))
                .px(px(8.0))
                .gap(px(8.0))
                .flex()
                .items_center()
                .rounded(px(6.0))
                .text_sm()
                .cursor_pointer()
                .hover(move |style| style.bg(tokens.sidebar_item_hover))
                .child(
                    svg()
                        .path("icons/folder-plus.svg")
                        .size(px(16.0))
                        .text_color(tokens.sidebar_icon),
                )
                .child("Add project")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.close_sidebar_project_picker(window, cx);
                    this.select_folder_and_add_project(window, cx);
                })),
        );
        if query.is_empty() {
            let folders = self.settings.sidebar.project_folders;
            for (id, label, active, multiline) in [
                ("sidebar-project-folders-option", "Project folders", folders, false),
                ("sidebar-project-multiline-option", "Multi-line folder rows", self.settings.sidebar.multiline_folder_rows, true),
            ] {
                if multiline && !folders { continue }
                panel = panel.child(div().id(id).debug_selector(move || id.into()).tab_index(0)
                    .role(gpui::Role::CheckBox).aria_label(label)
                    .aria_toggled(if active { gpui::Toggled::True } else { gpui::Toggled::False })
                    .flex().items_center().h(px(32.)).px(px(8.)).gap(px(8.)).rounded(px(6.))
                    .cursor_pointer().text_sm()
                    .hover(move |row| row.bg(tokens.sidebar_item_hover))
                    .focus_visible(move |row| row.bg(tokens.sidebar_item_hover))
                    .child(div().size(px(16.)).when(active, |row| {
                        row.child(svg().path("icons/status-check.svg").size(px(14.)).text_color(tokens.sidebar_icon))
                    }))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let sidebar = &mut this.settings.sidebar;
                        if multiline { sidebar.multiline_folder_rows = !sidebar.multiline_folder_rows; }
                        else { sidebar.project_folders = !sidebar.project_folders; }
                        this.sidebar_inbox_scroll.set_offset(Point::default());
                        this.schedule_workspace_save(cx);
                        cx.notify();
                    })));
            }
        }
        panel
    }

    fn render_new_session_project_modal(
        &self,
        picker: &SidebarProjectPicker,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let tokens = self.tokens(cx);
        let top = (window.viewport_size().height * 0.08).clamp(px(16.0), px(64.0));
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
            .track_scroll(&picker.scroll)
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
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.0))
                    .when(highlighted, |row| row.bg(tokens.sidebar_item_active))
                    .when(!highlighted, |row| row.hover(|row| row.bg(tokens.sidebar_item_hover)))
                    .cursor(CursorStyle::PointingHand)
                    .child(project_favicon(project, tokens, px(18.0)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_sm()
                                    .line_height(px(18.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .truncate()
                                    .child(project.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .line_height(px(16.0))
                                    .text_color(tokens.text_secondary)
                                    .truncate()
                                    .child(project.path.to_string_lossy().to_string()),
                            ),
                    )
                    .when(project_idx == self.active_project_idx, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(tokens.text_secondary)
                                .child("Current"),
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
                    .debug_selector(|| "new-session-project-empty".into())
                    .p(px(12.0))
                    .text_sm()
                    .text_color(tokens.text_secondary)
                    .child("No matching projects"),
            );
        }
        let hints = [
            (&["↑", "↓"][..], "Navigate"),
            (&["Enter"][..], "Select"),
            (&["Esc"][..], "Close"),
        ].map(
            |(keys, label)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(
                        div()
                            .flex()
                            .gap(px(3.0))
                            .children(keys.iter().map(|&key| {
                                div()
                                    .min_w(px(18.0))
                                    .px(px(4.0))
                                    .rounded(px(4.0))
                                    .bg(tokens.text_primary.opacity(0.08))
                                    .text_color(tokens.text_primary)
                                    .text_center()
                                    .child(key)
                            })),
                    )
                    .child(label)
            },
        );
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(top)
            .pb(px(12.0))
            .bg(tokens.window_bg.opacity(0.64))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_sidebar_project_picker(window, cx)),
            )
            .child(
                div()
                    .id("new-session-project-modal")
                    .debug_selector(|| "new-session-project-modal".into())
                    .w(px(560.0))
                    .max_w(relative(0.94))
                    .max_h(relative(1.0))
                    .occlude()
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.card_bg)
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .debug_selector(|| "new-session-project-search".into())
                            .flex()
                            .flex_none()
                            .items_center()
                            .h(px(46.0))
                            .pr(px(12.0))
                            .border_b_1()
                            .border_color(tokens.border_subtle)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        Input::new(&picker.input)
                                            .w_full()
                                            .px(px(15.0))
                                            .appearance(false)
                                            .prefix(
                                                svg()
                                                    .path("ui_icons/search.svg")
                                                    .size(px(14.0))
                                                    .flex_none()
                                                    .text_color(tokens.text_secondary),
                                            ),
                                    ),
                            )
                            .child(
                                sidebar_icon_button(
                                    "new-session-project-close",
                                    "icons/x.svg",
                                    13.0,
                                    &tokens,
                                    cx.listener(|this, _, window, cx| {
                                        this.close_sidebar_project_picker(window, cx);
                                    }),
                                )
                                .debug_selector(|| "new-session-project-close".into())
                                .tooltip(|window, cx| {
                                    gpui_component::tooltip::Tooltip::new("Close project picker")
                                        .build(window, cx)
                                }),
                            ),
                    )
                    .child(
                        div()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .px(px(8.0))
                            .py(px(6.0))
                            .child(
                                div()
                                    .flex_none()
                                    .px(px(8.0))
                                    .py(px(4.0))
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(tokens.text_secondary)
                                    .child("Projects"),
                            )
                            .child(list),
                    )
                    .child(
                        div()
                            .debug_selector(|| "new-session-project-hints".into())
                            .flex()
                            .flex_none()
                            .flex_wrap()
                            .items_center()
                            .gap(px(12.0))
                            .px(px(16.0))
                            .py(px(9.0))
                            .border_t_1()
                            .border_color(tokens.border_subtle)
                            .text_xs()
                            .line_height(px(18.0))
                            .text_color(tokens.text_secondary)
                            .children(hints),
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
        let tokens = self.sidebar_tokens(cx);
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
            .tab_index(0)
            .focus_visible(move |row| row.bg(tokens.sidebar_item_hover))
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
            row = row.on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                match mode {
                    SidebarProjectPickerMode::Scope => {
                        if let Some(index) = project_idx.filter(|_| event.modifiers().shift) {
                            this.toggle_sidebar_project_scope(index, cx);
                        } else {
                            this.set_sidebar_project_scope(project_idx, window, cx);
                        }
                    }
                    SidebarProjectPickerMode::NewSession => {
                        this.choose_new_session_project(project_idx, window, cx)
                    }
                }
            }));
        }
        row
    }

    fn render_sidebar_inbox(&self, tokens: ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let groups = self.sidebar_thread_groups();
        let active_key = self.sidebar_active_thread_key();
        let now_ms = sidebar_now_ms();
        let folders = self.settings.sidebar.project_folders;
        let mut list = div()
            .w_full()
            .min_h(gpui::relative(1.0))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(1.0));
        let dragging = cx.has_active_drag();
        if dragging {
            list = list.child(self.sidebar_drop_boundary("Pinned", SidebarSection::Pinned, tokens, cx));
        }
        for row in groups.pinned.iter().copied() {
            let item = self.render_sidebar_thread_card(row, active_key, now_ms, tokens, cx);
            list = list.child(self.sidebar_motion_row(item, self.sidebar_thread_id(row)));
        }
        if dragging {
            list = list.child(self.sidebar_drop_boundary("Active", SidebarSection::Active, tokens, cx));
        }
        if folders {
            let items = self.render_sidebar_project_folders(&groups.active, active_key, now_ms, tokens, cx);
            list = list.children(items);
        } else {
            for row in groups.active.iter().copied() {
                let item = self.render_sidebar_thread_card(row, active_key, now_ms, tokens, cx);
                list = list.child(self.sidebar_motion_row(item, self.sidebar_thread_id(row)));
            }
        }
        let all_active = self.projects.first()
            .is_some_and(|project| project.visible_session_count == usize::MAX);
        if !folders && (groups.hidden_active > 0 || all_active && groups.active.len() > SIDEBAR_ACTIVE_INITIAL_COUNT) {
            let button = div()
                    .id("sidebar-active-show-more")
                    .debug_selector(|| "sidebar-active-show-more".into())
                    .tab_index(0)
                    .h(px(32.0))
                    .flex_none()
                    .w_full()
                    .flex()
                    .items_center()
                    .px(px(10.0))
                    .gap(px(8.0))
                    .rounded(px(6.0))
                    .hover(move |row| row.bg(tokens.sidebar_item_hover))
                    .focus_visible(move |row| row.bg(tokens.sidebar_item_hover))
                    .child(
                        svg()
                            .path(if all_active {
                                "ui_icons/chevron-up.svg"
                            } else {
                                "icons/chevron-down.svg"
                            })
                            .size(px(16.0))
                            .text_color(tokens.sidebar_icon),
                    )
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .cursor(CursorStyle::PointingHand)
                    .child(if all_active { "Show less".into() } else { format!("Show {} more", groups.hidden_active) })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let limit = if all_active { SIDEBAR_ACTIVE_INITIAL_COUNT } else { usize::MAX };
                        this.set_sidebar_active_visible_count(limit, cx);
                    }));
            list = list.child(self.sidebar_nav_row(button, SidebarNavKey::More(None), None, cx));
        }
        if (!folders || self.projects.is_empty()) && groups.working_count == 0 && groups.pinned.is_empty() && groups.active.is_empty()
            && groups.snoozed.is_empty()
            && (groups.settled.is_empty() || !self.sidebar_scope_indices().is_empty())
        {
            list = list.child(self.render_sidebar_empty(!groups.settled.is_empty(), tokens, cx));
        }
        list = list.child(div().flex_1().min_h(px(8.0)));
        let mut shelves = u8::from(folders) << 6;
        if !folders && groups.working_count > 0 {
            let expanded = self.settings.sidebar.working_expanded;
            shelves |= 1 | (u8::from(expanded) << 1);
            let header = self.sidebar_shelf_header("sidebar-working-toggle", "Working", groups.working_count, expanded, tokens)
                .debug_selector(|| "sidebar-working-toggle".into())
                .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar_working(cx)));
            list = list.child(self.sidebar_nav_row(header, SidebarNavKey::Shelf("Working"), None, cx));
            for row in groups.working.iter().copied().filter(|row| expanded || row.revealed_active) {
                let item = self.render_sidebar_thread_card(row, active_key, now_ms, tokens, cx);
                list = list.child(self.sidebar_motion_row(item, self.sidebar_thread_id(row)));
            }
        }
        for (shelf, rows, expanded) in [
            (SidebarShelf::Snoozed, groups.snoozed.as_slice(), self.settings.sidebar.snoozed_expanded),
            (SidebarShelf::Settled, groups.settled.as_slice(), self.settings.sidebar.settled_expanded),
        ] {
            if !rows.is_empty() || expanded || dragging {
                let offset = if shelf == SidebarShelf::Snoozed { 2 } else { 4 };
                shelves |= (1 | (u8::from(expanded) << 1)) << offset;
                list = list.child(self.render_sidebar_shelf(
                    (shelf, expanded),
                    rows,
                    if shelf == SidebarShelf::Settled { groups.hidden_settled } else { 0 },
                    (active_key, now_ms),
                    tokens,
                    cx,
                ));
            }
        }
        self.sidebar_motion_list(list, !dragging, shelves, tokens, cx)
    }

    fn sidebar_drop_boundary(
        &self,
        label: &'static str,
        section: SidebarSection,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        div()
            .id(label)
            .h(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .rounded(px(6.0))
            .px(px(10.0))
            .text_xs()
            .text_color(tokens.sidebar_muted)
            .child(label)
            .drag_over::<SidebarThreadDrag>(move |row, _, _, _| row.bg(tokens.sidebar_item_active))
            .on_drop::<SidebarThreadDrag>(cx.listener(move |this, drag: &SidebarThreadDrag, _, cx| {
                cx.stop_propagation();
                this.drop_sidebar_threads(drag, section, None, cx);
            }))
    }

    fn render_sidebar_empty(
        &self,
        has_settled: bool,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let scoped = self.sidebar_scope_indices();
        let has_projects = !self.projects.is_empty();
        let label = if !has_projects {
            "No projects yet".into()
        } else if scoped.len() > 1 {
            if has_settled {
                "No active threads in these projects".into()
            } else {
                "No threads in these projects yet".into()
            }
        } else if let Some(&idx) = scoped.first() {
            if has_settled {
                format!("No active threads in {}", self.projects[idx].name)
            } else {
                format!("No threads in {} yet", self.projects[idx].name)
            }
        } else {
            "No threads yet".into()
        };
        div()
            .id("sidebar-search-empty")
            .debug_selector(|| "sidebar-search-empty".into())
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0))
            .py(px(24.0))
            .px(px(8.0))
            .text_xs()
            .text_center()
            .text_color(tokens.sidebar_muted)
            .child(label)
            .when(!has_projects || !scoped.is_empty(), |empty| {
                empty.child(
                    div()
                        .id("sidebar-empty-action")
                        .tab_index(0)
                        .px(px(10.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(tokens.sidebar_border)
                        .focus_visible(move |row| row.border_color(tokens.ring))
                        .hover(move |row| row.bg(tokens.sidebar_item_hover))
                        .cursor_pointer()
                        .child(if has_projects { "Show all projects" } else { "Add project" })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if has_projects {
                                this.set_sidebar_project_scope(None, window, cx);
                            } else {
                                this.select_folder_and_add_project(window, cx);
                            }
                        })),
                )
            })
    }


    fn sidebar_shelf_header(
        &self,
        id: &'static str,
        title: &'static str,
        count: usize,
        expanded: bool,
        tokens: ThemeTokens,
    ) -> gpui::Stateful<Div> {
        div()
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(title)
            .aria_expanded(expanded)
            .tab_index(0)
            .h(px(32.0))
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .text_xs()
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tokens.sidebar_muted)
            .border_1()
            .border_color(rgba(0))
            .focus_visible(move |row| row.border_color(tokens.ring))
            .hover(move |row| row.bg(tokens.sidebar_item_hover))
            .cursor_pointer()
            .child(
                svg()
                    .path(if expanded {
                        "icons/chevron-down.svg"
                    } else {
                        "icons/chevron-right.svg"
                    })
                    .size(px(14.0))
                    .text_color(tokens.sidebar_icon),
            )
            .child(title)
            .child(div().flex_1())
            .child(count.to_string())
    }

    fn render_sidebar_shelf(
        &self,
        (shelf, expanded): (SidebarShelf, bool),
        rows: &[SidebarThreadRef],
        hidden_settled: usize,
        (active_key, now_ms): (Option<(usize, usize)>, u64),
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (title, section) = match shelf {
            SidebarShelf::Snoozed => ("Snoozed", SidebarSection::Snoozed),
            SidebarShelf::Settled => ("Settled", SidebarSection::Settled),
        };
        let mut container = div()
            .id(match shelf {
                SidebarShelf::Snoozed => "sidebar-snoozed-shelf",
                SidebarShelf::Settled => "sidebar-settled-shelf",
            })
            .debug_selector(|| "sidebar-shelf".into())
            .w_full()
            .flex()
            .flex_col();
        let header = self.sidebar_shelf_header("sidebar-shelf-toggle", title, rows.len() + hidden_settled, expanded, tokens)
                    .drag_over::<SidebarThreadDrag>(move |row, _, _, _| row.bg(tokens.sidebar_item_active))
                    .on_drop::<SidebarThreadDrag>(cx.listener(move |this, drag: &SidebarThreadDrag, _, cx| {
                        cx.stop_propagation();
                        this.drop_sidebar_threads(drag, section, None, cx);
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_sidebar_shelf(shelf, cx)));
        container = container.child(self.sidebar_nav_row(header, SidebarNavKey::Shelf(title), None, cx));
        for row in rows.iter().copied().filter(|row| expanded || row.revealed_active) {
            let item = self.render_sidebar_thread_slim(row, active_key, now_ms, tokens, cx);
            container = container.child(self.sidebar_motion_row(item, self.sidebar_thread_id(row)));
        }
        if expanded && hidden_settled > 0 {
                container = container.child(
                    div()
                        .id("sidebar-settled-show-more")
                        .debug_selector(|| "project-show-more".into())
                        .tab_index(0)
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
        container
    }
}
