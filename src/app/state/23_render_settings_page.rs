impl AppState {
    fn render_settings_page(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let current_pref = self.theme_preference;
        let sidebar_docked =
            Self::sidebar_docked(self.sidebar_visible, window.viewport_size().width);
        let content_width = window.viewport_size().width
            - if sidebar_docked {
                self.sidebar_width
            } else {
                px(0.0)
            };
        let narrow = content_width < px(520.0);
        let settings_close = div()
            .id("settings-close-btn")
            .debug_selector(|| "settings-close-btn".to_string())
            .w(px(36.0))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .cursor(CursorStyle::PointingHand)
            .hover(move |style| style.bg(tokens.titlebar_btn_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.active_view = ActiveView::Workspace;
                    cx.notify();
                }),
            )
            .child(
                svg()
                    .path("icons/arrow-left.svg")
                    .size(px(14.0))
                    .text_color(tokens.titlebar_fg),
            );

        let titlebar = self
            .desktop_titlebar(cx)
            .h(px(38.0))
            .bg(tokens.titlebar_bg)
            .border_color(tokens.titlebar_border)
            .when(cfg!(target_os = "macos") && sidebar_docked, |bar| {
                bar.pl_3()
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .gap(px(8.0))
                    .when(!sidebar_docked, |row| row.pl(px(112.0)))
                    .child(settings_close)
                    .child(
                        svg()
                            .path("icons/settings.svg")
                            .size(px(14.0))
                            .text_color(tokens.titlebar_fg),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(tokens.titlebar_fg)
                            .child("Settings"),
                    ),
            );

        // Small, self-contained previews make each appearance recognizable before
        // it is selected, including the split light/dark system preview.
        let preview_half = |dark: bool| {
            let background = rgba(if dark { 0x111113ff } else { 0xfafafaff });
            let sidebar = rgba(if dark { 0x171719ff } else { 0xf4f4f5ff });
            let line = rgba(if dark { 0x303034ff } else { 0xdcdcdfff });
            div()
                .flex_1()
                .h_full()
                .flex()
                .bg(background)
                .overflow_hidden()
                .child(
                    div()
                        .w(px(37.0))
                        .h_full()
                        .p(px(7.0))
                        .bg(sidebar)
                        .flex()
                        .flex_col()
                        .gap(px(5.0))
                        .children(
                            (0..4).map(|_| div().h(px(7.0)).w_full().rounded_full().bg(line)),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .p(px(10.0))
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(div().w_full().h(px(8.0)).rounded_full().bg(line))
                        .child(div().w(relative(0.72)).h(px(6.0)).rounded_full().bg(line))
                        .child(div().flex_1())
                        .child(
                            div()
                                .w_full()
                                .h(px(15.0))
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(line)
                                .flex()
                                .items_center()
                                .justify_end()
                                .pr(px(3.0))
                                .child(div().size(px(8.0)).rounded_full().bg(rgba(0x818cf8ff))),
                        ),
                )
        };
        let options = [
            (ThemePreference::System, "theme-option-system", "System"),
            (ThemePreference::Light, "theme-option-light", "Light"),
            (ThemePreference::Dark, "theme-option-dark", "Dark"),
        ];
        let cards = options
            .into_iter()
            .map(|(preference, id, title)| {
                let selected = current_pref == preference;
                let preview = div()
                    .w_full()
                    .h(px(112.0))
                    .flex()
                    .rounded(px(6.0))
                    .overflow_hidden()
                    .when(preference != ThemePreference::Dark, |el| {
                        el.child(preview_half(false))
                    })
                    .when(preference != ThemePreference::Light, |el| {
                        el.child(preview_half(true))
                    });
                div()
                    .id(id)
                    .flex_1()
                    .min_w(px(0.0))
                    .p(px(7.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(if selected {
                        tokens.ring
                    } else {
                        tokens.card_border
                    })
                    .bg(tokens.card_bg)
                    .cursor(CursorStyle::PointingHand)
                    .hover(move |style| style.bg(tokens.card_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.set_theme_preference(preference, Some(window), cx);
                        }),
                    )
                    .child(preview)
                    .child(
                        div()
                            .w_full()
                            .pt(px(5.0))
                            .text_center()
                            .text_size(px(12.0))
                            .text_color(if selected {
                                tokens.text_primary
                            } else {
                                tokens.text_secondary
                            })
                            .child(title),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let navigation = [
            (
                SettingsSection::Appearance,
                "settings-nav-appearance",
                "icons/settings.svg",
                "Appearance",
            ),
            (
                SettingsSection::Data,
                "settings-nav-data",
                "icons/folder.svg",
                "Data & sessions",
            ),
            (
                SettingsSection::Updates,
                "settings-nav-updates",
                "icons/settings.svg",
                "Updates",
            ),
        ]
        .into_iter()
        .map(|(section, id, icon, label)| {
            let selected = self.settings_section == section;
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(7.0))
                .when(selected, |item| item.bg(tokens.card_hover))
                .hover(move |item| item.bg(tokens.card_hover))
                .cursor(CursorStyle::PointingHand)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings_section = section;
                    this.clear_data_confirm = false;
                    cx.notify();
                }))
                .child(
                    svg()
                        .path(icon)
                        .size(px(15.0))
                        .text_color(tokens.text_secondary),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(tokens.text_primary)
                        .child(label),
                )
        })
        .collect::<Vec<_>>();

        let content = match self.settings_section {
            SettingsSection::Appearance => div()
                .flex()
                .flex_col()
                .gap(px(16.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(tokens.text_secondary)
                        .child("Choose how Shob looks across your workspace."),
                )
                .child(
                    div()
                        .pt(px(12.0))
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(tokens.text_primary)
                        .child("Color scheme"),
                )
                .child(
                    div()
                        .flex()
                        .when(narrow, |row| row.flex_col())
                        .gap(px(12.0))
                        .children(cards),
                )
                .child(self.render_palette_scheme_picker(narrow, cx))
                .child(self.render_theme_file_section(cx)),
            SettingsSection::Data => self.render_data_section(cx),
            SettingsSection::Updates => self.render_updates(cx),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(tokens.window_bg)
            .child(titlebar)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .when(narrow, |layout| layout.flex_col())
                    .child(
                        div()
                            .when(!narrow, |nav| nav.w(px(168.0)).h_full().border_r_1())
                            .when(narrow, |nav| nav.w_full().border_b_1())
                            .border_color(tokens.card_border)
                            .bg(tokens.titlebar_bg)
                            .p(px(8.0))
                            .flex()
                            .when(!narrow, |nav| nav.flex_col())
                            .gap(px(2.0))
                            .children(navigation),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_y_scrollbar()
                            .px(if narrow { px(20.0) } else { px(40.0) })
                            .py(px(32.0))
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(728.0))
                                    .mx_auto()
                                    .flex()
                                    .flex_col()
                                    .gap(px(20.0))
                                    .child(
                                        div()
                                            .text_size(px(26.0))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(tokens.text_primary)
                                            .child(match self.settings_section {
                                                SettingsSection::Appearance => "Appearance",
                                                SettingsSection::Data => "Data & sessions",
                                                SettingsSection::Updates => "Updates",
                                            }),
                                    )
                                    .child(content),
                            ),
                    ),
            )
    }

    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.update_state = updater::State::Checking;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(updater::check).await;
            let _ = this.update(cx, |this, cx| {
                this.update_state =
                    result.unwrap_or_else(|error| updater::State::Failed(error.to_string()));
                cx.notify();
            });
        })
        .detach();
    }

    fn activate_update(&mut self, window: &Window, cx: &mut Context<Self>) {
        use updater::State;
        match &mut self.update_state {
            State::Available(_) => {
                let State::Available(update) =
                    std::mem::replace(&mut self.update_state, State::Installing)
                else {
                    unreachable!();
                };
                cx.notify();
                cx.spawn(async move |this, cx| {
                    let result = smol::unblock(move || updater::apply(*update)).await;
                    let _ = this.update(cx, |this, cx| {
                        this.update_state = match result {
                            Ok(ready) => State::Ready(ready),
                            Err(error) => State::Failed(error.to_string()),
                        };
                        cx.notify();
                    });
                })
                .detach();
            }
            #[cfg(windows)]
            State::Ready(ready) => {
                let path = ready.path.clone();
                if let Some(temp) = ready.installer_files.take() {
                    // Windows Installer needs these files after Shob exits.
                    let _ = temp.keep();
                }
                self.save_all_sync(Some(window), cx);
                cx.set_restart_path(path);
                cx.restart();
            }
            #[cfg(unix)]
            State::Ready(ready) => {
                let path = ready.path.clone();
                let app_image = ready.app_image;
                self.save_all_sync(Some(window), cx);
                self.update_state = State::Restarting;
                cx.notify();
                cx.spawn(async move |this, cx| {
                    let result =
                        smol::unblock(move || updater::schedule_restart(&path, app_image)).await;
                    let _ = this.update(cx, |this, cx| match result {
                        Ok(()) => cx.quit(),
                        Err(error) => {
                            this.update_state = State::Failed(error.to_string());
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            #[cfg(unix)]
            State::Restarting => {}
            State::Checking | State::Installing => {}
            _ if updater::enabled() => self.check_for_updates(cx),
            _ => {}
        }
    }

    fn render_updates(&self, cx: &mut Context<Self>) -> gpui::Div {
        use updater::State;
        let tokens = self.tokens(cx);
        let (message, action) = match &self.update_state {
            State::Idle if !updater::enabled() => (
                "In-app updates are available in official release builds.".into(),
                None,
            ),
            State::Idle => (
                "Check GitHub for a new version.".into(),
                Some("Check for updates"),
            ),
            State::Checking => ("Checking for updates…".into(), None),
            State::Current => ("Shob is up to date.".into(), Some("Check for updates")),
            State::Available(update) => (
                format!("Shob {} is available.", update.version()),
                Some("Download update"),
            ),
            State::Installing => ("Downloading and preparing the update…".into(), None),
            State::Ready(ready) => (
                format!(
                    "Shob {} is ready. Restart when you can close your running terminals.",
                    ready.version
                ),
                Some("Restart to update"),
            ),
            #[cfg(unix)]
            State::Restarting => ("Restarting Shob…".into(), None),
            State::External(message) => (message.clone(), Some("Check again")),
            State::Failed(error) => (format!("Update failed. {error}"), Some("Try again")),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(tokens.text_primary)
                    .child(concat!("Shob ", env!("CARGO_PKG_VERSION"))),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(tokens.text_secondary)
                    .child(message),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.0))
                    .children(action.map(|label| {
                        Button::new("shob-update-action").label(label).on_click(
                            cx.listener(|this, _, window, cx| this.activate_update(window, cx)),
                        )
                    }))
                    .child(
                        Button::new("shob-release-notes")
                            .label("View releases")
                            .on_click(|_, _, cx| cx.open_url(updater::RELEASES)),
                    ),
            )
    }

    fn render_theme_file_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(14.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(tokens.card_border)
            .bg(tokens.card_bg)
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(tokens.text_primary)
                    .child("Theme settings file"),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(tokens.text_secondary)
                    .child("Edit settings.json, save it, then reload to apply your theme."),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("open-theme-settings")
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .bg(tokens.secondary_btn_bg)
                            .text_color(tokens.secondary_btn_fg)
                            .cursor(CursorStyle::PointingHand)
                            .on_click(cx.listener(|_, _, _, _| {
                                if let Err(error) =
                                    paths::open_in_file_manager(&paths::settings_path())
                                {
                                    eprintln!("failed to open settings file: {error}");
                                }
                            }))
                            .child("Open settings.json"),
                    )
                    .child(
                        div()
                            .id("reload-theme-settings")
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .bg(tokens.secondary_btn_bg)
                            .text_color(tokens.secondary_btn_fg)
                            .cursor(CursorStyle::PointingHand)
                            .on_click(cx.listener(|this, _, window, cx| {
                                match settings::reload() {
                                    Ok(settings) => {
                                        this.settings.theme_colors = settings.theme_colors;
                                        this.theme_preference = settings.theme;
                                        this.palette_scheme = settings.palette_scheme;
                                        this.apply_theme_preference(Some(window), cx);
                                    }
                                    Err(error) => window.push_notification(
                                        Notification::new()
                                            .title("Could not reload settings")
                                            .message(error)
                                            .with_type(NotificationType::Error),
                                        cx,
                                    ),
                                }
                            }))
                            .child("Reload theme"),
                    ),
            )
    }

    /// Data directory, restore toggles and the "clear saved sessions" action.
    fn render_data_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let restore = self.settings.restore.clone();
        let toggles = [
            (
                "restore-sessions",
                "Restore sessions on launch",
                "Reopen projects, tabs, splits, focus and working directories.",
                restore.sessions,
            ),
            (
                "restore-scrollback",
                "Restore terminal scrollback",
                "Seed the previous screen text above the fresh shell prompt.",
                restore.scrollback,
            ),
            (
                "restore-agent-history",
                "Save agent history",
                "Keep conversation transcripts and resume shob sessions after a restart.",
                restore.agent_history,
            ),
        ];
        let toggle_rows = toggles
            .into_iter()
            .map(|(id, title, subtitle, on)| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .px(px(14.0))
                    .py(px(10.0))
                    .border_b_1()
                    .border_color(tokens.card_border)
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_restore_option(id, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(tokens.text_primary)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(tokens.text_secondary)
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        div()
                            .w(px(34.0))
                            .h(px(20.0))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .when(on, |switch| switch.justify_end())
                            .p(px(2.0))
                            .rounded_full()
                            .bg(if on {
                                tokens.ring
                            } else {
                                tokens.pill_inactive_bg
                            })
                            .child(div().size(px(16.0)).rounded_full().bg(tokens.text_primary)),
                    )
            })
            .collect::<Vec<_>>();

        let archived_threads = self.sidebar_archived_thread_refs();
        let archived_rows = archived_threads
            .into_iter()
            .map(|(project_idx, session_idx, title)| {
                div()
                    .id(ElementId::Name(
                        format!("archived-thread-{project_idx}-{session_idx}").into(),
                    ))
                    .debug_selector(|| "archived-thread".into())
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(7.0))
                    .border_t_1()
                    .border_color(tokens.card_border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(px(12.0))
                            .text_color(tokens.text_primary)
                            .child(title),
                    )
                    .child(
                        div()
                            .id(ElementId::Name(
                                format!("restore-thread-{project_idx}-{session_idx}").into(),
                            ))
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(tokens.card_border)
                            .text_size(px(11.0))
                            .text_color(tokens.text_primary)
                            .cursor(CursorStyle::PointingHand)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.restore_sidebar_thread(project_idx, session_idx, cx);
                            }))
                            .child("Restore"),
                    )
            })
            .collect::<Vec<_>>();
        let has_archived_rows = !archived_rows.is_empty();

        let confirm = self.clear_data_confirm;
        let data_dir = paths::data_dir();
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(tokens.text_secondary)
                            .child("Shob data and SHOB sessions are stored in this folder."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(tokens.card_bg)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(tokens.text_secondary)
                                    .child("App data folder"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(tokens.text_primary)
                                    .child(data_dir.display().to_string()),
                            ),
                    )
                    .child(
                        div()
                            .id("open-data-folder")
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(tokens.card_border)
                            .text_size(px(12.0))
                            .text_color(tokens.text_primary)
                            .cursor(CursorStyle::PointingHand)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_data_folder(cx);
                            }))
                            .child("Open folder"),
                    ),
            )
            .child(
                div()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(tokens.card_bg)
                    .overflow_hidden()
                    .children(toggle_rows),
            )
            .child(
                div()
                    .id("sidebar-settings")
                    .debug_selector(|| "sidebar-settings".into())
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(tokens.card_bg)
                    .overflow_hidden()
                    .child(
                        div()
                            .px(px(14.0))
                            .pt(px(12.0))
                            .pb(px(8.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(tokens.text_primary)
                                    .child("Sidebar"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(tokens.text_secondary)
                                    .child("Restore archived threads here."),
                            ),
                    )
                    .when(has_archived_rows, move |card| {
                        card.child(
                            div()
                                .px(px(14.0))
                                .pt(px(10.0))
                                .pb(px(6.0))
                                .text_size(px(11.0))
                                .text_color(tokens.text_secondary)
                                .child("Archived threads"),
                        )
                        .children(archived_rows)
                    }),
            )
            .child(
                div()
                    .id("clear-saved-data")
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.card_border)
                    .bg(tokens.card_bg)
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clear_saved_data(cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(tokens.block_failure)
                                    .child("Clear saved sessions & history"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(tokens.text_secondary)
                                    .child("Deletes saved layouts, scrollback and Shob-owned SHOB sessions. Settings are kept."),
                            ),
                    )
                    .child(
                        div()
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded_full()
                            .bg(tokens.pill_inactive_bg)
                            .text_size(px(11.0))
                            .text_color(if confirm {
                                tokens.block_failure
                            } else {
                                tokens.text_secondary
                            })
                            .child(if confirm { "Confirm clear" } else { "Clear" }),
                    ),
            )
    }
}
