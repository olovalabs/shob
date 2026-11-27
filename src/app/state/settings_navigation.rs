struct SettingsPage {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    icon: &'static str,
}

impl SettingsSection {
    const GROUPS: [(&str, &[Self]); 3] = [
        ("Personal", &[Self::Appearance, Self::Themes]),
        ("Workspace", &[Self::Restore, Self::Archive, Self::Storage]),
        ("Application", &[Self::Browser, Self::Updates]),
    ];

    fn page(self) -> SettingsPage {
        let (id, title, description, icon) = match self {
            Self::Appearance => (
                "settings-nav-appearance",
                "Appearance",
                "Choose a light or dark appearance, or follow your system.",
                "icons/sun.svg",
            ),
            Self::Themes => (
                "settings-nav-themes",
                "Themes",
                "Choose a palette for Shob and the terminal.",
                "icons/sparkle.svg",
            ),
            Self::Restore => (
                "settings-nav-restore",
                "Session Restore",
                "Choose what Shob brings back when you reopen it.",
                "icons/clock.svg",
            ),
            Self::Archive => (
                "settings-nav-archive",
                "Archived Threads",
                "Restore archived threads to your workspace sidebar.",
                "icons/archive.svg",
            ),
            Self::Storage => (
                "settings-nav-storage",
                "Storage",
                "Manage local data, scrollback, and saved history.",
                "icons/folder.svg",
            ),
            Self::Browser => (
                "settings-nav-browser",
                "Browser",
                "Install optional browser support for @browser in Agent chat.",
                "icons/monitor.svg",
            ),
            Self::Updates => (
                "settings-nav-updates",
                "Updates",
                "Check for a new version of Shob.",
                "icons/arrow-down.svg",
            ),
        };
        SettingsPage {
            id,
            title,
            description,
            icon,
        }
    }
}

impl AppState {
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_sidebar_search(window, cx);
        self.close_sidebar_project_picker(window, cx);
        self.close_sidebar_thread_menu(cx);
        self.close_terminal_context_menu(cx);
        self.close_history_search(window, cx);
        self.compact_sidebar_open = false;
        self.clear_data_confirm = false;
        self.active_view = ActiveView::Settings;
        self.settings_focus.focus(window, cx);
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_data_confirm = false;
        self.active_view = ActiveView::Workspace;
        if let Some(pane_id) = self.focused_pane_id {
            self.focus_pane(pane_id, window, cx);
        }
        cx.notify();
    }

    fn render_settings_page(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        let compact = window.viewport_size().width < px(720.0);
        let narrow_content = window.viewport_size().width < px(if compact { 540.0 } else { 850.0 });
        let section = self.settings_section;
        let page = section.page();
        let content = match section {
            SettingsSection::Appearance => self.render_appearance_section(narrow_content, cx),
            SettingsSection::Themes => self.render_themes_section(narrow_content, cx),
            SettingsSection::Restore => self.render_restore_section(cx),
            SettingsSection::Archive => self.render_archive_section(cx),
            SettingsSection::Storage => self.render_storage_section(cx),
            SettingsSection::Browser => self.render_browser_section(cx),
            SettingsSection::Updates => self.render_updates(cx),
        };
        let titlebar = self
            .desktop_titlebar(cx)
            .h(px(42.0))
            .bg(tokens.titlebar_bg)
            .border_color(tokens.card_border)
            .child(
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(9.0))
                    .child(
                        div()
                            .id("settings-close-btn")
                            .debug_selector(|| "settings-close-btn".into())
                            .tab_index(0)
                            .occlude()
                            .h(px(30.0))
                            .px(px(8.0))
                            .rounded(px(7.0))
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .text_size(px(12.0))
                            .text_color(tokens.text_secondary)
                            .cursor_pointer()
                            .hover(move |button| {
                                button.bg(tokens.card_hover).text_color(tokens.text_primary)
                            })
                            .focus_visible(move |button| button.bg(tokens.sidebar_item_active))
                            .child(
                                svg()
                                    .path("icons/arrow-left.svg")
                                    .size(px(14.0))
                                    .text_color(tokens.text_secondary),
                            )
                            .child("Workspace")
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_settings(window, cx)),
                            ),
                    )
                    .when(compact, |bar| {
                        bar.child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Settings"),
                        )
                    }),
            );
        div()
            .id("settings-root")
            .track_focus(&self.settings_focus)
            .tab_group()
            .size_full()
            .flex()
            .flex_col()
            .bg(tokens.window_bg)
            .text_color(tokens.text_primary)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = &event.keystroke;
                let shortcut =
                    key.modifiers.control || cfg!(target_os = "macos") && key.modifiers.platform;
                if key.key == "escape" && this.clear_data_confirm {
                    this.clear_data_confirm = false;
                    cx.stop_propagation();
                    cx.notify();
                } else if key.key == "escape" || shortcut && !key.modifiers.alt && key.key == "," {
                    cx.stop_propagation();
                    this.close_settings(window, cx);
                }
            }))
            .child(titlebar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(compact, |layout| layout.flex_col())
                    .child(self.render_settings_navigation(compact, tokens, cx))
                    .child(
                        div()
                            .id(("settings-scroll", section as usize))
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .child(
                                div().size_full().overflow_y_scrollbar().child(
                                    div()
                                        .w_full()
                                        .max_w(px(800.0))
                                        .mx_auto()
                                        .px(px(if compact { 16.0 } else { 36.0 }))
                                        .py(px(if compact { 20.0 } else { 30.0 }))
                                        .flex()
                                        .flex_col()
                                        .gap(px(22.0))
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap(px(5.0))
                                                .child(
                                                    div()
                                                        .text_size(px(25.0))
                                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                                        .child(page.title),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(13.0))
                                                        .text_color(tokens.text_secondary)
                                                        .child(page.description),
                                                ),
                                        )
                                        .child(content),
                                ),
                            ),
                    ),
            )
    }

    fn render_settings_navigation(
        &self,
        compact: bool,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let rail = div()
            .id("settings-navigation")
            .debug_selector(|| "settings-navigation".into())
            .flex_shrink_0()
            .bg(tokens.sidebar_bg)
            .border_color(tokens.sidebar_border);
        if compact {
            rail.w_full()
                .border_b_1()
                .p(px(8.0))
                .grid()
                .grid_cols(3)
                .gap(px(4.0))
                .children(
                    SettingsSection::GROUPS
                        .iter()
                        .flat_map(|(_, sections)| sections.iter().copied())
                        .map(|section| self.render_settings_link(section, true, tokens, cx)),
                )
        } else {
            rail.w(px(214.0))
                .h_full()
                .border_r_1()
                .p(px(12.0))
                .flex()
                .flex_col()
                .gap(px(17.0))
                .overflow_y_scroll()
                .child(
                    div()
                        .px(px(9.0))
                        .py(px(4.0))
                        .text_size(px(17.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Settings"),
                )
                .children(
                    SettingsSection::GROUPS
                        .into_iter()
                        .map(|(label, sections)| {
                            div()
                                .flex()
                                .flex_col()
                                .flex_shrink_0()
                                .gap(px(3.0))
                                .child(
                                    div()
                                        .px(px(10.0))
                                        .pb(px(4.0))
                                        .text_size(px(11.0))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(tokens.text_secondary)
                                        .child(label),
                                )
                                .children(sections.iter().map(|&section| {
                                    self.render_settings_link(section, false, tokens, cx)
                                }))
                        }),
                )
        }
    }

    fn render_settings_link(
        &self,
        section: SettingsSection,
        compact: bool,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let page = section.page();
        let selected = self.settings_section == section;
        let label = match (compact, section) {
            (true, SettingsSection::Restore) => "Restore",
            (true, SettingsSection::Archive) => "Archive",
            _ => page.title,
        };
        let icon_color = if selected {
            tokens.ring
        } else {
            tokens.text_secondary
        };
        div()
            .id(page.id)
            .debug_selector(move || page.id.into())
            .tab_index(0)
            .relative()
            .min_w_0()
            .h(px(if compact { 32.0 } else { 36.0 }))
            .px(px(9.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .cursor_pointer()
            .when(selected, |item| item.bg(tokens.sidebar_item_active))
            .hover(move |item| {
                item.bg(if selected {
                    tokens.sidebar_item_active
                } else {
                    tokens.sidebar_item_hover
                })
            })
            .focus_visible(move |item| item.bg(tokens.card_hover))
            .child(
                svg()
                    .path(page.icon)
                    .size(px(15.0))
                    .flex_shrink_0()
                    .text_color(icon_color),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.0))
                    .text_color(tokens.text_primary)
                    .child(label),
            )
            .when(
                section == SettingsSection::Updates && self.update_state.needs_attention(),
                |item| {
                    item.child(div().flex_1()).child(
                        div()
                            .size(px(6.0))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(tokens.ring),
                    )
                },
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings_section = section;
                this.clear_data_confirm = false;
                cx.notify();
            }))
    }
}
