impl AppState {
    fn render_sidebar_thread_card(
        &self,
        row: SidebarThreadRef,
        active_key: Option<(usize, usize)>,
        now_ms: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        let Some(session) = self
            .projects
            .get(row.project_idx)
            .and_then(|project| project.sessions.get(row.session_idx))
        else {
            return div().id(("missing-sidebar-card", row.session_idx));
        };
        let project = &self.projects[row.project_idx];
        let active = row.revealed_active
            || self.sidebar_thread_is(row, active_key.unwrap_or((usize::MAX, usize::MAX)));
        let (status, status_color) = self.sidebar_thread_status(session, now_ms, tokens);
        let branch = session
            .panes
            .first()
            .and_then(|pane| pane.chat.workspace_override.as_ref().or(pane.cwd.as_ref()))
            .and_then(|path| self.git_snapshots.get(path))
            .or_else(|| self.git_snapshots.get(&project.path))
            .and_then(|snapshot| snapshot.current_branch.clone());
        let draft = self.sidebar_has_unsent_draft(session, cx);
        let row_id = format!("session-row-{}", session.id);
        let project_idx = row.project_idx;
        let session_idx = row.session_idx;
        div()
            .id(ElementId::Name(row_id.clone().into()))
            .debug_selector(move || row_id.clone())
            .group("sidebar-thread-card")
            .relative()
            .h(SIDEBAR_THREAD_CARD_HEIGHT)
            .w_full()
            .flex()
            .flex_col()
            .justify_center()
            .px(px(8.0))
            .rounded(px(7.0))
            .when(active, |style| {
                style
                    .bg(tokens.sidebar_item_active)
                    .text_color(tokens.sidebar_item_active_fg)
            })
            .hover(|style| style.bg(tokens.sidebar_item_hover))
            .cursor(CursorStyle::PointingHand)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .child(project_favicon(project, tokens, px(16.0)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(project.name.clone()),
                    )
                    .when(row.pinned, |parent| {
                        parent.child(svg().path("ui_icons/pin.svg").size(px(12.0)))
                    })
                    .child(self.sidebar_thread_status_menu(
                        row,
                        status,
                        status_color,
                        "sidebar-thread-card",
                        tokens,
                        cx,
                    )),
            )
            .child(
                div().w_full().flex().items_center().mt(px(3.0)).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(session.title.clone()),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .mt(px(2.0))
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(branch.unwrap_or_default()),
                    )
                    .when(draft, |parent| parent.child("Draft"))
                    .child(session_logo_icon(session.logo.clone(), tokens.sidebar_icon)),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_session(project_idx, session_idx, window, cx);
            }))
    }

    fn render_sidebar_thread_slim(
        &self,
        row: SidebarThreadRef,
        active_key: Option<(usize, usize)>,
        now_ms: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        let Some(session) = self
            .projects
            .get(row.project_idx)
            .and_then(|project| project.sessions.get(row.session_idx))
        else {
            return div().id(("missing-sidebar-slim", row.session_idx));
        };
        let active = row.revealed_active
            || self.sidebar_thread_is(row, active_key.unwrap_or((usize::MAX, usize::MAX)));
        let (status, status_color) = self.sidebar_thread_status(session, now_ms, tokens);
        let row_id = format!("session-row-{}", session.id);
        let project_idx = row.project_idx;
        let session_idx = row.session_idx;
        div()
            .id(ElementId::Name(row_id.clone().into()))
            .debug_selector(move || row_id.clone())
            .group("sidebar-thread-slim")
            .relative()
            .h(SIDEBAR_THREAD_SLIM_HEIGHT)
            .w_full()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(8.0))
            .rounded(px(7.0))
            .when(active, |style| {
                style
                    .bg(tokens.sidebar_item_active)
                    .text_color(tokens.sidebar_item_active_fg)
            })
            .hover(|style| style.bg(tokens.sidebar_item_hover))
            .cursor(CursorStyle::PointingHand)
            .child(project_favicon(&self.projects[project_idx], tokens, px(16.0)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .child(session.title.clone()),
            )
            .child(session_logo_icon(session.logo.clone(), tokens.sidebar_icon))
            .child(self.sidebar_thread_status_menu(
                row,
                status,
                status_color,
                "sidebar-thread-slim",
                tokens,
                cx,
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_session(project_idx, session_idx, window, cx);
            }))
    }

    fn sidebar_thread_status_menu(
        &self,
        row: SidebarThreadRef,
        status: String,
        status_color: Hsla,
        group: &'static str,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let menu_open = self.sidebar_thread_menu.is_some_and(|menu| {
            menu.project_idx == row.project_idx && menu.session_idx == row.session_idx
        });
        div()
            .relative()
            .min_w(px(24.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_end()
            .child(
                div()
                    .text_xs()
                    .text_color(status_color)
                    .child(status)
                    .when(menu_open, |label| label.invisible())
                    .group_hover(group, |label| label.invisible()),
            )
            .child(
                div()
                    .absolute()
                    .right_0()
                    .flex()
                    .items_center()
                    .when(row.section != SidebarSection::Settled, |actions| {
                        actions.child(
                            div()
                                .id(ElementId::Name(
                                    format!(
                                        "sidebar-quick-settle-{}-{}",
                                        row.project_idx, row.session_idx
                                    )
                                    .into(),
                                ))
                                .debug_selector(|| "sidebar-quick-settle".into())
                                .h(px(24.0))
                                .px(px(5.0))
                                .rounded(px(6.0))
                                .flex()
                                .items_center()
                                .gap(px(3.0))
                                .text_xs()
                                .text_color(tokens.sidebar_muted)
                                .cursor(CursorStyle::PointingHand)
                                .hover(|button| button.bg(tokens.sidebar_item_active))
                                .child(svg().path("icons/status-check.svg").size(px(12.0)))
                                .child("Settle")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.settle_sidebar_thread(
                                        row.project_idx,
                                        row.session_idx,
                                        cx,
                                    );
                                }))
                                .when(!menu_open, |button| button.invisible())
                                .group_hover(group, |button| button.visible()),
                        )
                    })
                    .child(
                        self.sidebar_thread_menu(
                            row.project_idx,
                            row.session_idx,
                            row.section,
                            row.pinned,
                            tokens,
                            cx,
                        )
                        .when(!menu_open, |button| button.invisible())
                        .group_hover(group, |button| button.visible()),
                    ),
            )
    }

    fn sidebar_thread_menu(
        &self,
        project_idx: usize,
        session_idx: usize,
        section: SidebarSection,
        pinned: bool,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(ElementId::Name(
                format!("sidebar-thread-menu-{project_idx}-{session_idx}").into(),
            ))
            .debug_selector(|| "sidebar-thread-menu".into())
            .size(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .hover(|button| button.bg(tokens.sidebar_item_active))
            .cursor(CursorStyle::PointingHand)
            .child(
                svg()
                    .path("icons/ellipsis.svg")
                    .size(px(14.0))
                    .text_color(tokens.sidebar_muted),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.open_sidebar_thread_menu(
                    project_idx,
                    session_idx,
                    section,
                    pinned,
                    window,
                    cx,
                );
            }))
    }

    fn open_sidebar_thread_menu(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        section: SidebarSection,
        pinned: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_thread_menu = Some(SidebarThreadMenuState {
            project_idx,
            session_idx,
            section,
            pinned,
        });
        cx.notify();
    }

    fn sidebar_thread_menu_row(
        &self,
        id: &'static str,
        label: &'static str,
        icon: &'static str,
        tokens: ThemeTokens,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .h(px(30.0))
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .text_xs()
            .text_color(tokens.sidebar_fg)
            .hover(|style| style.bg(tokens.sidebar_item_hover))
            .cursor(CursorStyle::PointingHand)
            .child(
                svg()
                    .path(icon)
                    .size(px(13.0))
                    .text_color(tokens.sidebar_muted),
            )
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                action(this, window, cx);
                this.close_sidebar_thread_menu(cx);
            }))
    }

    fn render_sidebar_thread_menu_overlay(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(menu) = self.sidebar_thread_menu else {
            return div();
        };
        let tokens = self.tokens(cx);
        let project_idx = menu.project_idx;
        let session_idx = menu.session_idx;
        let section = menu.section;
        let pinned = menu.pinned;
        let mut popover = div()
            .id("sidebar-thread-menu-popover")
            .debug_selector(|| "sidebar-thread-menu-popover".into())
            .absolute()
            .top(px(88.0))
            .right(px(8.0))
            .w(px(184.0))
            .p(px(6.0))
            .rounded(px(10.0))
            .bg(tokens.card_bg)
            .border_1()
            .border_color(tokens.border)
            .shadow_md()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .occlude();
        popover = popover.child(self.sidebar_thread_menu_row(
            "sidebar-thread-pin",
            if pinned { "Unpin" } else { "Pin" },
            "ui_icons/pin.svg",
            tokens,
            move |this, _, cx| this.toggle_sidebar_thread_pin(project_idx, session_idx, cx),
            cx,
        ));
        if pinned {
            for (id, label, icon, delta) in [
                (
                    "sidebar-thread-move-up",
                    "Move up",
                    "icons/arrow-up.svg",
                    -1isize,
                ),
                (
                    "sidebar-thread-move-down",
                    "Move down",
                    "icons/arrow-down.svg",
                    1isize,
                ),
            ] {
                popover = popover.child(self.sidebar_thread_menu_row(
                    id,
                    label,
                    icon,
                    tokens,
                    move |this, _, cx| {
                        this.move_sidebar_thread(project_idx, session_idx, delta, cx)
                    },
                    cx,
                ));
            }
        }
        popover = popover.child(self.sidebar_thread_menu_row(
            "sidebar-thread-settle",
            if section == SidebarSection::Settled {
                "Return to inbox"
            } else {
                "Settle"
            },
            "icons/status-check.svg",
            tokens,
            move |this, _, cx| {
                if section == SidebarSection::Settled {
                    this.unsettle_sidebar_thread(project_idx, session_idx, cx);
                } else {
                    this.settle_sidebar_thread(project_idx, session_idx, cx);
                }
            },
            cx,
        ));
        popover = popover.child(self.sidebar_thread_menu_row(
            "sidebar-thread-snooze",
            if section == SidebarSection::Snoozed {
                "Wake"
            } else {
                "Snooze 1 day"
            },
            "icons/clock.svg",
            tokens,
            move |this, _, cx| {
                if section == SidebarSection::Snoozed {
                    this.wake_sidebar_thread(project_idx, session_idx, cx);
                } else {
                    this.snooze_sidebar_thread(project_idx, session_idx, cx);
                }
            },
            cx,
        ));
        popover = popover.child(self.sidebar_thread_menu_row(
            "sidebar-thread-archive",
            "Archive",
            "icons/archive.svg",
            tokens,
            move |this, _, cx| this.archive_sidebar_thread(project_idx, session_idx, cx),
            cx,
        ));
        popover = popover.child(self.sidebar_thread_menu_row(
            "session-close",
            "Delete thread",
            "icons/pane-close.svg",
            tokens,
            move |this, window, cx| this.close_session(project_idx, session_idx, window, cx),
            cx,
        ));
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                div()
                    .id("sidebar-thread-menu-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.close_sidebar_thread_menu(cx)),
                    ),
            )
            .child(popover)
    }

    fn render_system_monitor(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        let stats = self.system_snapshot;
        let percent = |value: Option<u8>| {
            value
                .map(|value| format!("{value}%"))
                .unwrap_or_else(|| "Unavailable".into())
        };
        let memory = if stats.memory_total == 0 {
            "Unavailable".to_string()
        } else {
            format!(
                "{:.1} / {:.1} GiB",
                stats.memory_used as f64 / 1_073_741_824.0,
                stats.memory_total as f64 / 1_073_741_824.0
            )
        };
        let row = |label: &'static str, value: String, detail: Option<String>| {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(5.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(12.0))
                                .text_color(tokens.text_secondary)
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(tokens.text_primary)
                                .child(value),
                        ),
                )
                .when_some(detail, |this, detail| {
                    this.child(
                        div()
                            .text_size(px(10.0))
                            .text_color(tokens.text_muted)
                            .child(detail),
                    )
                })
        };
        div()
            .id("system-monitor-panel")
            .debug_selector(|| "system-monitor-panel".into())
            .absolute()
            .bottom(px(46.0))
            .right(px(8.0))
            .w(px(230.0))
            .max_w(relative(0.95))
            .p(px(14.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(tokens.border)
            .bg(tokens.card_bg)
            .shadow_md()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .occlude()
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(tokens.text_primary)
                            .child("System monitor"),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(tokens.text_muted)
                            .child("LIVE"),
                    ),
            )
            .child(row("CPU", percent(stats.cpu), None))
            .child(row(
                "Memory",
                memory,
                stats.memory.map(|value| format!("{value}% in use")),
            ))
            .child(row(
                "GPU",
                percent(stats.gpu),
                stats
                    .gpu
                    .is_none()
                    .then(|| "Utilization is not exposed by this driver".into()),
            ))
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        div()
            .id("sidebar-footer")
            .debug_selector(|| "sidebar-footer".into())
            .h(px(40.0))
            .w_full()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .bg(tokens.sidebar_footer_bg)
            .border_t_1()
            .border_color(tokens.sidebar_footer_border)
            .child(
                sidebar_icon_action(
                    "layout-switch",
                    "icons/bot.svg",
                    if self.settings.agent_layout {
                        "Agent mode on · Switch to split view"
                    } else {
                        "Agent mode off · Switch to Agent mode"
                    },
                    self.settings.agent_layout,
                    tokens,
                )
                .on_click(cx.listener(|this, _, window, cx| this.switch_agent_layout(window, cx))),
            )
            .child(div().flex_1())
            .child(
                sidebar_icon_action(
                    "system-monitor",
                    "icons/monitor.svg",
                    "System monitor",
                    self.system_monitor_open,
                    tokens,
                )
                .debug_selector(|| "system-monitor".into())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.system_monitor_open = !this.system_monitor_open;
                    cx.notify();
                })),
            )
            .child(
                sidebar_icon_action(
                    "sidebar-settings",
                    "icons/settings.svg",
                    "Settings",
                    self.update_state.needs_attention(),
                    tokens,
                )
                .debug_selector(|| "sidebar-settings-entry".into())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_settings(window, cx);
                })),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        div()
            .id("sidebar-root")
            .debug_selector(|| "sidebar".into())
            .relative()
            .h_full()
            .w(self.sidebar_width)
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(tokens.sidebar_bg)
            .border_r_1()
            .border_color(tokens.sidebar_border)
            .child(self.render_sidebar_actions(cx))
            .when(self.integration_panel_visible, |this| {
                this.child(self.render_integrations_panel(cx))
            })
    }

}
