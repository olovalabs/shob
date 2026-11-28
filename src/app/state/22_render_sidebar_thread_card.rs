const SIDEBAR_THREAD_ICON_SIZE: Pixels = px(16.0);
// The SVG viewBox leaves room around the body for motion.
const SIDEBAR_BOT_ART_SIZE: Pixels = px(21.0);

fn sidebar_bot_icon(avatar: impl IntoElement) -> Div {
    div()
        .size(SIDEBAR_THREAD_ICON_SIZE)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(avatar)
}

impl AppState {
    fn start_sidebar_thread_rename(
        &mut self,
        session_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(&(project, session)) = self.session_index.get(&session_id) else {
            return;
        };
        let title = self.sidebar_thread_title(&self.projects[project].sessions[session]).clone();
        let previous_focus = window.focused(cx);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(title));
        let focus = input.read(cx).focus_handle().clone();
        let blur = cx.on_blur(&focus, window, |this, window, cx| {
            this.finish_sidebar_thread_rename(true, window, cx);
        });
        let submit = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.finish_sidebar_thread_rename(true, window, cx);
            }
        });
        self.sidebar_thread_rename = Some(SidebarThreadRename {
            session_id,
            input: input.clone(),
            previous_focus,
            _blur: blur,
            _submit: submit,
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all_text(cx);
        });
        cx.notify();
    }

    fn finish_sidebar_thread_rename(
        &mut self,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rename) = self.sidebar_thread_rename.take() else {
            return;
        };
        let input = rename.input.read(cx);
        let focused = input.focus_handle().is_focused(window);
        if commit
            && let Some(&(project, session)) = self.session_index.get(&rename.session_id)
        {
            let session = &self.projects[project].sessions[session];
            let value = input.value();
            let title = value.trim();
            if !title.is_empty() && title != self.sidebar_thread_title(session).as_str() {
                let uid = session.uid.clone();
                let title = (title != self.sidebar_default_thread_title(session).as_str())
                    .then(|| SharedString::from(title));
                self.sidebar_threads.entry(uid).or_default().title = title;
                self.schedule_workspace_save(cx);
            }
        }
        if focused && let Some(previous) = rename.previous_focus {
            window.focus(&previous, cx);
        }
        cx.notify();
    }

    fn sidebar_thread_rename_focused(&self, window: &Window, cx: &App) -> bool {
        self.sidebar_thread_rename
            .as_ref()
            .is_some_and(|rename| rename.input.read(cx).focus_handle().is_focused(window))
    }

    fn sidebar_thread_rename_handles_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.sidebar_thread_rename_focused(window, cx) {
            return false;
        }
        if key == "escape" {
            cx.stop_propagation();
            self.finish_sidebar_thread_rename(false, window, cx);
        }
        true
    }

    fn render_sidebar_thread_title(&self, session: &Session, tokens: ThemeTokens) -> Div {
        let title = self.sidebar_thread_title(session);
        let title_element = div().flex_1().min_w_0().text_sm().line_height(px(20.0));
        if let Some(rename) = self.sidebar_thread_rename.as_ref().filter(|r| r.session_id == session.id) {
            title_element.child(
                sidebar_input(&rename.input, tokens).appearance(false).cleanable(false),
            )
        } else {
            title_element.truncate().font_weight(gpui::FontWeight::MEDIUM).child(title.clone())
        }
    }

    fn sidebar_row_interactions(
        &self,
        element: gpui::Stateful<Div>,
        row: SidebarThreadRef,
        session: &Session,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let session_id = session.id;
        let key = SidebarNavKey::Thread(session_id);
        let in_search = self.sidebar_navigation.borrow().search;
        let parent = if in_search {
            None
        } else if !self.settings.sidebar.project_folders && session.status == SessionStatus::Working
            && row.section != SidebarSection::Settled {
            Some(SidebarNavKey::Shelf("Working"))
        } else {
            match row.section {
                SidebarSection::Active if self.settings.sidebar.project_folders =>
                    Some(SidebarNavKey::Folder(self.projects[row.project_idx].id)),
                SidebarSection::Snoozed => Some(SidebarNavKey::Shelf("Snoozed")),
                SidebarSection::Settled => Some(SidebarNavKey::Shelf("Settled")),
                _ => None,
            }
        };
        let selected = self.sidebar_selection.members.contains(&session_id);
        let element = self.sidebar_nav_row(element, key, parent, cx);
        let renaming = self.sidebar_thread_rename.as_ref().is_some_and(|r| r.session_id == session_id);
        element
            .role(gpui::Role::Button)
            .aria_label(self.sidebar_thread_title(session).clone())
            .when(row.revealed_active, |row| row.aria_description("Current thread"))
            .aria_selected(row.revealed_active || selected)
            .when(selected, |row| row.bg(tokens.sidebar_item_active).text_color(tokens.sidebar_item_active_fg))
            .border_1()
            .border_color(rgba(0))
            .when(row.revealed_active && !tokens.is_dark, |row| row.border_color(tokens.sidebar_border))
            .focus_visible(move |row| row.border_color(tokens.ring))
            .when(!renaming, |element| {
                element
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if this.select_sidebar_thread(session_id, event.modifiers(), cx) {
                            return;
                        }
                        if event.click_count() == 2 && !event.modifiers().modified() {
                            this.start_sidebar_thread_rename(session_id, window, cx);
                        } else {
                            this.select_session(row.project_idx, row.session_idx, window, cx);
                        }
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_sidebar_thread_menu(row, event.position, window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "f2" {
                            this.start_sidebar_thread_rename(session_id, window, cx);
                            cx.stop_propagation();
                        } else if sidebar_context_key(&event.keystroke) {
                            cx.stop_propagation();
                        }
                    }))
                    .on_key_up(cx.listener(move |this, event: &gpui::KeyUpEvent, window, cx| {
                        if sidebar_context_key(&event.keystroke) {
                            let position = this.sidebar_row_bounds(key).map(|bounds| bounds.center())
                                .unwrap_or_else(|| window.mouse_position());
                            this.open_sidebar_thread_menu(row, position, window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_drag(
                        SidebarThreadDrag {
                            session_id,
                            title: self.sidebar_thread_title(session).clone(),
                            tokens,
                            members: self.sidebar_selection.members.clone(),
                        },
                        |drag, _, _, cx| {
                            cx.new(|_| SidebarThreadDrag {
                                session_id: drag.session_id,
                                title: drag.title.clone(),
                                tokens: drag.tokens,
                                members: drag.members.clone(),
                            })
                        },
                    )
                    .drag_over::<SidebarThreadDrag>(move |style, drag, _, _| {
                        if drag.session_id == session_id || drag.members.contains(&drag.session_id) && drag.members.contains(&session_id) {
                            style
                        } else {
                            style.border_color(tokens.ring)
                        }
                    })
                    .on_drop::<SidebarThreadDrag>(cx.listener(move |this, drag: &SidebarThreadDrag, window, cx| {
                        cx.stop_propagation();
                        let before = this.sidebar_row_bounds(key).is_some_and(|bounds| window.mouse_position().y < bounds.center().y);
                        this.drop_sidebar_threads(drag, row.section, Some((row, before)), cx);
                    }))
            })
    }

    fn render_sidebar_thread_card(
        &self,
        row: SidebarThreadRef,
        active_key: Option<(usize, usize)>,
        now_ms: u64,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let Some(session) = self
            .projects
            .get(row.project_idx)
            .and_then(|project| project.sessions.get(row.session_idx))
        else {
            return div().id(("missing-sidebar-card", row.session_idx));
        };
        if self.bots.contains_key(&session.uid) {
            return self.render_sidebar_compact_row(row, active_key, now_ms, tokens, false, cx);
        }
        let project = &self.projects[row.project_idx];
        let active = row.revealed_active
            || self.sidebar_thread_is(row, active_key.unwrap_or((usize::MAX, usize::MAX)));
        let (status, status_color) = self.sidebar_thread_status(session, now_ms, tokens);
        let branch = session
            .panes
            .first()
            .and_then(|pane| pane.chat.workspace_override.as_ref().or(pane.cwd.as_ref()))
            .and_then(|path| self.git_snapshots.get(path))
            .or_else(|| self.git_snapshots.get(project.session_dir(session)))
            .and_then(|snapshot| snapshot.current_branch.as_deref());
        let draft = self.sidebar_has_unsent_draft(session, cx);
        let row_id = format!("session-row-{}", session.id);
        let element = div()
            .id(ElementId::Name(row_id.clone().into()))
            .debug_selector(move || row_id.clone())
            .group("sidebar-thread-card")
            .relative()
            .h(if branch.is_some() || draft { SIDEBAR_THREAD_CARD_HEIGHT } else { px(60.0) })
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .justify_center()
            .px(px(9.0))
            .rounded(px(6.0))
            .when(active, |style| {
                style
                    .bg(tokens.sidebar_item_active)
                    .text_color(tokens.sidebar_item_active_fg)
            })
            .when(!active, |row| row.hover(move |style| style.bg(tokens.sidebar_item_hover)))
            .cursor(CursorStyle::PointingHand)
            .child(
                div()
                    .flex()
                    .h(px(20.0))
                    .items_center()
                    .gap(px(6.0))
                    .text_xs()
                    .text_color(tokens.sidebar_muted)
                    .child(project_favicon(project, tokens, SIDEBAR_THREAD_ICON_SIZE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(project.name.clone()),
                    )
                    .when(row.pinned, |parent| {
                        parent.child(
                            svg()
                                .path("ui_icons/pin.svg")
                                .size(px(12.0))
                                .text_color(tokens.sidebar_icon),
                        )
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
                    self.render_sidebar_thread_title(session, tokens),
                ),
            )
            .when(branch.is_some() || draft, |card| {
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .mt(px(2.0))
                        .text_xs()
                        .text_color(tokens.sidebar_muted)
                        .child(div().flex_1().min_w_0().truncate().child(
                            branch.map(SharedString::from).unwrap_or_default(),
                        ))
                        .when(draft, |parent| parent.child("Draft"))
                        .child(session_logo_icon(session.logo.clone(), tokens.sidebar_icon)),
                )
            });
        self.sidebar_row_interactions(element, row, session, tokens, cx)
    }

    fn render_sidebar_thread_slim(
        &self,
        row: SidebarThreadRef,
        active_key: Option<(usize, usize)>,
        now_ms: u64,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        self.render_sidebar_compact_row(row, active_key, now_ms, tokens, false, cx)
    }

    fn render_sidebar_folder_thread(
        &self,
        row: SidebarThreadRef,
        active_key: Option<(usize, usize)>,
        now_ms: u64,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        self.render_sidebar_compact_row(row, active_key, now_ms, tokens, true, cx)
    }

    fn render_sidebar_compact_row(
        &self,
        row: SidebarThreadRef,
        active_key: Option<(usize, usize)>,
        now_ms: u64,
        tokens: ThemeTokens,
        folder: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
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
        let bot = self.bots.get(&session.uid);
        let row_id = format!("session-row-{}", session.id);
        let project_idx = row.project_idx;
        let branch = (bot.is_none() && folder && self.settings.sidebar.multiline_folder_rows)
            .then(|| self.git_snapshots.get(self.projects[project_idx].session_dir(session))
                .and_then(|snapshot| snapshot.current_branch.as_deref()))
            .flatten();
        let element = div()
            .id(ElementId::Name(row_id.clone().into()))
            .debug_selector(move || row_id.clone())
            .group("sidebar-thread-slim")
            .relative()
            .h(if branch.is_some() { px(54.) } else { SIDEBAR_THREAD_SLIM_HEIGHT })
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(9.0))
            .when(folder, |row| row.pl(px(36.)))
            .rounded(px(6.0))
            .when(active, |style| {
                style
                    .bg(tokens.sidebar_item_active)
                    .text_color(tokens.sidebar_item_active_fg)
            })
            .when(!active, |row| row.hover(move |style| style.bg(tokens.sidebar_item_hover)))
            .cursor(CursorStyle::PointingHand)
            .when(!folder && bot.is_none(), |row| {
                row.child(project_favicon(&self.projects[project_idx], tokens, SIDEBAR_THREAD_ICON_SIZE))
            })
            .when_some(bot, |row, bot| {
                row.debug_selector(|| "custom-bot-row".into())
                    .child(sidebar_bot_icon(bot_avatar::sidebar_avatar("avatar", bot.avatar, SIDEBAR_BOT_ART_SIZE)))
            })
            .child(div().flex_1().min_w_0().flex().flex_col()
                .child(self.render_sidebar_thread_title(session, tokens))
                .when_some(branch, |title, branch| {
                    title.child(div().text_xs().truncate().text_color(tokens.sidebar_muted).child(branch.to_owned()))
                }))
            .when(bot.is_none(), |row| {
                row.child(session_logo_icon(session.logo.clone(), tokens.sidebar_icon))
            })
            .child(self.sidebar_thread_status_menu(
                row,
                status,
                status_color,
                "sidebar-thread-slim",
                tokens,
                cx,
            ));
        self.sidebar_row_interactions(element, row, session, tokens, cx)
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
        let bot = self.bots.contains_key(&self.projects[row.project_idx].sessions[row.session_idx].uid);
        let menu_open = self.sidebar_thread_menu.is_some_and(|menu| {
            menu.row.project_idx == row.project_idx && menu.row.session_idx == row.session_idx
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
                    .when(!bot && row.section != SidebarSection::Snoozed, |actions| {
                        let label = if row.section == SidebarSection::Settled { "Un-settle thread" } else { "Settle thread" };
                        let button = div()
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
                                .child(
                                    svg()
                                        .path("icons/status-check.svg")
                                        .size(px(12.0))
                                        .text_color(tokens.sidebar_icon),
                                )
                                .child(if row.section == SidebarSection::Settled {
                                    "Un-settle"
                                } else {
                                    "Settle"
                                })
                                .role(gpui::Role::Button)
                                .aria_label(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if row.section == SidebarSection::Settled {
                                        this.unsettle_sidebar_thread(row.project_idx, row.session_idx, cx);
                                    } else {
                                        this.settle_sidebar_thread(row.project_idx, row.session_idx, cx);
                                    }
                                }))
                                .when(!menu_open, |button| button.invisible())
                                .group_hover(group, |button| button.visible());
                        actions.child(self.sidebar_tooltip(
                            button, ("sidebar-settle-tooltip", self.sidebar_thread_id(row)), label,
                            SidebarTooltipSide::Right, tokens, cx,
                        ))
                    })
                    .child(
                        self.sidebar_tooltip(
                            self.sidebar_thread_menu(row, bot, tokens, cx)
                                .when(!menu_open, |button| button.invisible())
                                .group_hover(group, |button| button.visible()),
                            ("sidebar-thread-actions-tooltip", self.sidebar_thread_id(row)),
                            if bot { "Bot actions" } else { "Thread actions" },
                            SidebarTooltipSide::Right,
                            tokens,
                            cx,
                        ),
                    ),
            )
    }

    fn sidebar_thread_menu(
        &self,
        row: SidebarThreadRef,
        bot: bool,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(ElementId::Name(
                format!("sidebar-thread-menu-{}-{}", row.project_idx, row.session_idx).into(),
            ))
            .debug_selector(move || {
                if bot { "custom-bot-actions".into() } else { "sidebar-thread-menu".into() }
            })
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
            .role(gpui::Role::Button)
            .aria_label(if bot { "Bot actions" } else { "Thread actions" })
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                cx.stop_propagation();
                this.open_sidebar_thread_menu(row, event.position(), window, cx);
            }))
    }

    fn open_sidebar_thread_menu(
        &mut self,
        row: SidebarThreadRef,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_folder_menu = None;
        self.sidebar_thread_menu = Some(SidebarThreadMenuState { row, position });
        window.focus(&self.sidebar_menu_focus, cx);
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
            .tab_index(0)
            .role(gpui::Role::MenuItem)
            .aria_label(label)
            .focus_visible(move |style| style.bg(tokens.sidebar_item_hover))
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
                this.sidebar_custom_snooze = None;
                action(this, window, cx);
                if this.sidebar_custom_snooze.is_none() {
                    this.close_sidebar_thread_menu(cx);
                }
            }))
    }

    fn sidebar_custom_snooze_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((input, invalid)) = self.sidebar_custom_snooze.as_mut() else { return false };
        if !input.read(cx).focus_handle().is_focused(window) { return false }
        match key {
            "escape" => {
                self.sidebar_custom_snooze = None;
                window.focus(&self.sidebar_menu_focus, cx);
            }
            "enter" => {
                let duration = input.read(cx).value().trim().parse::<u64>().ok()
                    .filter(|minutes| *minutes > 0)
                    .and_then(|minutes| minutes.checked_mul(60_000));
                if let Some((duration, menu)) = duration.zip(self.sidebar_thread_menu) {
                    let origin = self.sidebar_thread_id(menu.row);
                    self.sidebar_bulk_action(origin, cx, |this, p, s, cx| this.snooze_sidebar_thread(p, s, duration, cx));
                    self.close_sidebar_thread_menu(cx);
                } else {
                    *invalid = true;
                }
            }
            _ => return true,
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    fn sidebar_context_panel(
        &self,
        id: &'static str,
        label: &'static str,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        div().id(id).debug_selector(move || id.into())
            .role(gpui::Role::Menu).aria_label(label).tab_group()
            .w(px(218.)).p(px(6.)).rounded(px(8.)).bg(tokens.card_bg)
            .border_1().border_color(tokens.border).shadow_md()
            .max_h(px(340.)).overflow_y_scroll()
            .flex().flex_col().gap(px(2.)).occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.close_sidebar_thread_menu(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "up" => window.focus_prev(cx),
                    "down" => window.focus_next(cx),
                    "home" => window.focus(&this.sidebar_menu_focus, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
    }

    fn render_sidebar_thread_menu_overlay(
        &self,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(menu) = self.sidebar_thread_menu else {
            return div().into_any_element();
        };
        let SidebarThreadRef { project_idx, session_idx, section, pinned, .. } = menu.row;
        let session_id = self.sidebar_thread_id(menu.row);
        let Some(session) = self
            .projects
            .get(project_idx)
            .and_then(|project| project.sessions.get(session_idx))
        else {
            return div().into_any_element();
        };
        let bot = self.bots.get(&session.uid);
        let is_bot = bot.is_some();
        let bulk = self.sidebar_action_selection(session_id, &self.sidebar_selection.members).len() > 1;
        let mut popover = self.sidebar_context_panel("sidebar-thread-menu-popover", "Thread actions", tokens, cx);
        if !bulk {
            popover = popover.child(self.sidebar_thread_menu_row(
                "sidebar-thread-rename", "Rename thread", "icons/pencil.svg", tokens,
                move |this, window, cx| this.start_sidebar_thread_rename(session_id, window, cx), cx,
            ).track_focus(&self.sidebar_menu_focus));
            if let Some(bot) = bot {
                let id = bot.id.clone();
                popover = popover.child(self.sidebar_thread_menu_row(
                    "custom-bot-edit", "Edit bot", "icons/bot.svg", tokens,
                    move |this, window, cx| this.open_bot_form(Some(&id), window, cx), cx,
                ));
            }
        }
        popover = popover.child(self.sidebar_thread_menu_row(
            "sidebar-thread-pin",
            match (bulk, pinned) {
                (true, true) => "Unpin selected threads",
                (true, false) => "Pin selected threads",
                (false, true) => "Unpin thread",
                (false, false) => "Pin thread",
            },
            "ui_icons/pin.svg",
            tokens,
            move |this, _, cx| this.sidebar_bulk_action(session_id, cx, |this, p, s, cx| {
                let uid = &this.projects[p].sessions[s].uid;
                if this.sidebar_thread_state(uid).pinned == pinned {
                    this.toggle_sidebar_thread_pin(p, s, cx);
                }
            }),
            cx,
        ).when(bulk, |row| row.track_focus(&self.sidebar_menu_focus)));
        if pinned && !bulk {
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
            match (bulk, section == SidebarSection::Settled) {
                (true, true) => "Un-settle selected threads",
                (true, false) => "Settle selected threads",
                (false, true) => "Un-settle thread",
                (false, false) => "Settle thread",
            },
            "icons/status-check.svg",
            tokens,
            move |this, _, cx| this.sidebar_bulk_action(session_id, cx, |this, p, s, cx| {
                if section == SidebarSection::Settled {
                    this.unsettle_sidebar_thread(p, s, cx);
                } else {
                    this.settle_sidebar_thread(p, s, cx);
                }
            }),
            cx,
        ));
        if section == SidebarSection::Snoozed {
            popover = popover.child(self.sidebar_thread_menu_row(
                "sidebar-thread-snooze", if bulk { "Wake selected threads" } else { "Wake thread" },
                "icons/clock.svg", tokens,
                move |this, _, cx| this.sidebar_bulk_action(session_id, cx, Self::wake_sidebar_thread), cx,
            ));
        } else {
            for (id, label, minutes) in [
                ("sidebar-snooze-hour", "Snooze 1 hour", 60),
                ("sidebar-thread-snooze", "Snooze 1 day", 24 * 60),
                ("sidebar-snooze-week", "Snooze 1 week", 7 * 24 * 60),
            ] {
                popover = popover.child(self.sidebar_thread_menu_row(
                    id, label, "icons/clock.svg", tokens,
                    move |this, _, cx| this.sidebar_bulk_action(session_id, cx, |this, p, s, cx| {
                        this.snooze_sidebar_thread(p, s, minutes * 60_000, cx);
                    }), cx,
                ));
            }
            if let Some((input, invalid)) = &self.sidebar_custom_snooze {
                popover = popover.child(div().id("sidebar-custom-snooze-field").role(gpui::Role::Group)
                    .aria_label("Snooze duration in minutes")
                    .px(px(8.)).py(px(4.)).flex().flex_col()
                    .child(sidebar_input(input, tokens).appearance(false).cleanable(false))
                    .when(*invalid, |row| row.text_xs().text_color(tokens.block_failure).child("Enter a positive number of minutes.")));
            } else {
                popover = popover.child(self.sidebar_thread_menu_row(
                    "sidebar-custom-snooze", "Custom snooze…", "icons/clock.svg", tokens,
                    |this, window, cx| {
                        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Minutes · Enter to snooze"));
                        input.update(cx, |input, cx| input.focus(window, cx));
                        this.sidebar_custom_snooze = Some((input, false));
                        cx.notify();
                    }, cx,
                ));
            }
        }
        if !bulk {
            let project = &self.projects[project_idx];
            let session = &project.sessions[session_idx];
            let workspace = session.panes.first()
                .and_then(|pane| pane.chat.workspace_override.as_ref().or(pane.cwd.as_ref()))
                .unwrap_or_else(|| project.session_dir(session));
            let branch = self.git_snapshots.get(workspace).and_then(|git| git.current_branch.as_ref());
            for (id, label, value) in [
                ("sidebar-copy-path", "Copy path", Some(workspace.to_string_lossy().into_owned())),
                ("sidebar-copy-branch", "Copy branch", branch.cloned()),
                ("sidebar-copy-thread-id", "Copy thread ID", Some(session.uid.clone())),
            ] {
                if let Some(value) = value {
                    popover = popover.child(self.sidebar_thread_menu_row(
                        id, label, "icons/copy.svg", tokens,
                        move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(value.clone())), cx,
                    ));
                }
            }
            let scoped = self.sidebar_scope_indices() == [project_idx];
            popover = popover.child(self.sidebar_thread_menu_row(
                "sidebar-filter-thread-project", if scoped { "Show all projects" } else { "Filter by project" },
                "icons/folder.svg", tokens,
                move |this, window, cx| this.set_sidebar_project_scope((!scoped).then_some(project_idx), window, cx),
                cx,
            ));
        }
        popover = popover.child(self.sidebar_thread_menu_row(
            "sidebar-thread-archive",
            if bulk { "Archive selected threads" } else { "Archive thread" },
            "icons/archive.svg",
            tokens,
            move |this, _, cx| this.sidebar_bulk_action(session_id, cx, Self::archive_sidebar_thread),
            cx,
        ));
        popover = popover.child(self.sidebar_thread_menu_row(
            "session-close",
            if bulk {
                "Delete selected threads"
            } else if is_bot {
                "Delete bot"
            } else {
                "Delete thread"
            },
            "icons/pane-close.svg",
            tokens,
            move |this, window, cx| {
                let ids = this.sidebar_action_selection(session_id, &this.sidebar_selection.members);
                let answer = window.prompt(
                    gpui::PromptLevel::Warning,
                    if bulk {
                        "Delete selected threads?"
                    } else if is_bot {
                        "Delete bot?"
                    } else {
                        "Delete thread?"
                    },
                    Some(if bulk {
                        "Selected threads and their panes will be permanently deleted, along with any associated bots."
                    } else if is_bot {
                        "This permanently deletes the bot and its dedicated chat."
                    } else {
                        "This permanently deletes the thread and its panes."
                    }),
                    &[
                        gpui::PromptButton::Cancel("Cancel".into()),
                        gpui::PromptButton::Ok("Delete".into()),
                    ],
                    cx,
                );
                cx.spawn_in(window, async move |this, cx| {
                    if answer.await == Ok(1) {
                        let _ = this.update_in(cx, |this, window, cx| {
                            for id in ids {
                                if let Some(&(project, session)) = this.session_index.get(&id) {
                                    this.close_session(project, session, window, cx);
                                }
                            }
                        });
                    }
                })
                .detach();
            },
            cx,
        ));
        gpui::anchored()
            .position(menu.position)
            .snap_to_window_with_margin(px(8.0))
            .child(popover)
            .into_any_element()
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

    fn render_sidebar_footer(&self, tokens: ThemeTokens, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id("sidebar-footer")
            .debug_selector(|| "sidebar-footer".into())
            .h(px(44.0))
            .w_full()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .child(
                self.sidebar_tooltip(
                sidebar_icon_action(
                    "sidebar-settings",
                    "icons/settings.svg",
                    "Settings",
                    self.update_state.needs_attention(),
                    tokens,
                )
                .debug_selector(|| "sidebar-settings-entry".into())
                .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
                    "sidebar-settings", "Settings", SidebarTooltipSide::Top, tokens, cx,
                ),
            )
            .child(
                self.sidebar_tooltip(
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
                    "layout-switch", "Switch workspace layout", SidebarTooltipSide::Top, tokens, cx,
                ),
            )
            .child(div().flex_1())
            .child(
                self.sidebar_tooltip(
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
                    "system-monitor", "System monitor", SidebarTooltipSide::Top, tokens, cx,
                ),
            )
    }

    fn render_sidebar(&self, tokens: ThemeTokens, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id("sidebar-root")
            .track_focus(&self.sidebar_focus)
            .tab_stop(false)
            .role(gpui::Role::Navigation)
            .aria_label("Threads")
            .debug_selector(|| "sidebar".into())
            .relative()
            .h_full()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(tokens.sidebar_bg)
            .text_color(tokens.sidebar_fg)
            .border_r_1()
            .border_color(tokens.sidebar_border)
            .child(self.render_sidebar_actions(tokens, cx))
            .when(self.integration_panel_visible, |this| {
                this.child(self.render_integrations_panel(cx))
            })
    }

}
