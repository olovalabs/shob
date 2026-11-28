enum BrowserInstallState {
    Checking,
    Unsupported,
    NotInstalled,
    Installing,
    Installed(PathBuf, Option<String>),
    Removing,
    Failed(String),
}

impl BrowserInstallState {
    fn path(&self) -> Option<&Path> {
        match self {
            Self::Installed(path, _) => Some(path),
            _ => None,
        }
    }
}

impl AppState {
    fn render_appearance_section(&self, narrow: bool, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let current_pref = self.theme_preference;

        let light = self
            .settings
            .theme_colors
            .tokens(gpui_component::ThemeMode::Light, self.palette_scheme);
        let dark = self
            .settings
            .theme_colors
            .tokens(gpui_component::ThemeMode::Dark, self.palette_scheme);
        let preview_half = |preview: ThemeTokens| {
            let line = preview.text_primary.opacity(0.12);
            div()
                .flex_1()
                .h_full()
                .flex()
                .bg(preview.pane_bg)
                .overflow_hidden()
                .child(
                    div()
                        .w(px(if narrow { 18.0 } else { 37.0 }))
                        .h_full()
                        .p(px(7.0))
                        .bg(preview.sidebar_bg)
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
                                .child(
                                    div()
                                        .size(px(8.0))
                                        .rounded_full()
                                        .bg(preview.model_picker_accent),
                                ),
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
                    .h(px(if narrow { 72.0 } else { 112.0 }))
                    .flex()
                    .rounded(px(6.0))
                    .overflow_hidden()
                    .when(preference != ThemePreference::Dark, |el| {
                        el.child(preview_half(light))
                    })
                    .when(preference != ThemePreference::Light, |el| {
                        el.child(preview_half(dark))
                    });
                div()
                    .id(id)
                    .debug_selector(move || id.into())
                    .tab_index(0)
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_theme_preference(preference, Some(window), cx);
                    }))
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

        div().flex().gap(px(10.0)).children(cards)
    }

    fn render_themes_section(&self, narrow: bool, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(px(18.0))
            .child(self.render_palette_scheme_picker(narrow, cx))
            .child(self.render_theme_file_section(cx))
    }

    fn check_browser_install(&mut self, cx: &mut Context<Self>) {
        if !browser::supported() {
            self.browser_install = BrowserInstallState::Unsupported;
            return;
        }
        cx.spawn(async move |this, cx| {
            let installed = smol::unblock(browser::installed).await;
            let _ = this.update(cx, |this, cx| {
                this.browser_install = match installed {
                    Ok(Some(path)) => BrowserInstallState::Installed(path, None),
                    Ok(None) => BrowserInstallState::NotInstalled,
                    Err(error) => BrowserInstallState::Failed(format!("{error:#}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn install_browser(&mut self, cx: &mut Context<Self>) {
        use BrowserInstallState::*;
        if !matches!(self.browser_install, NotInstalled | Failed(_)) {
            return;
        }
        self.browser_install = Installing;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(browser::install).await;
            let _ = this.update(cx, |this, cx| {
                this.browser_install = match result {
                    Ok(path) => Installed(path, None),
                    Err(error) => Failed(format!("{error:#}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn remove_browser(&mut self, cx: &mut Context<Self>) {
        use BrowserInstallState::*;
        let Installed(path, _) = &self.browser_install else {
            return;
        };
        let path = path.clone();
        self.browser_install = Removing;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (path, result) = smol::unblock(move || {
                let result = browser::remove(&path);
                (path, result)
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.browser_install = match result {
                    Ok(()) => NotInstalled,
                    Err(error) => Installed(path, Some(format!("{error:#}"))),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn render_browser_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        use BrowserInstallState::*;
        use gpui_component::button::ButtonVariants as _;
        let tokens = self.tokens(cx);
        let native = crate::browser_automation::SUPPORTED;
        let message: SharedString = match &self.browser_install {
            Checking => "Checking for browser support…".into(),
            Unsupported => "Browser installation is unavailable on this platform.".into(),
            NotInstalled => {
                "Not installed. Setup downloads agent-browser and Chrome for Testing.".into()
            }
            Installing => "Downloading and installing browser support…".into(),
            Installed(_, None) if native => "The old agent-browser helper is no longer needed.".into(),
            Installed(_, None) => format!(
                "Installed · agent-browser {}. Use @browser in Agent chat.",
                browser::VERSION
            )
            .into(),
            Installed(_, Some(error)) => format!("Removal failed. {error}").into(),
            Removing => "Removing agent-browser…".into(),
            Failed(error) => format!("Installation failed. {error}").into(),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .when(native, |page| {
                page.child(
                    div()
                        .text_size(px(13.0))
                        .text_color(tokens.text_secondary)
                        .child("Start an Agent chat request with @browser to control a private tab in the right-hand Browser pane. The browser engine downloads on first use."),
                )
            })
            .when(!native || matches!(self.browser_install, Installed(..) | Removing), |page| {
                page.child(
                    div()
                        .text_size(px(13.0))
                        .text_color(tokens.text_secondary)
                        .child(message),
                )
            })
            .when(
                !native && matches!(self.browser_install, NotInstalled | Failed(_)),
                |page| {
                    page.child(
                        div()
                            .debug_selector(|| "browser-install-action".into())
                            .child(
                                Button::new("install-browser")
                                    .label(if matches!(self.browser_install, Failed(_)) {
                                        "Try again"
                                    } else {
                                        "Install browser"
                                    })
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.install_browser(cx);
                                    })),
                            ),
                    )
                },
            )
            .when(matches!(self.browser_install, Installed(..)), |page| {
                page.child(
                    Button::new("remove-browser-helper")
                        .label("Remove helper")
                        .on_click(cx.listener(|this, _, _, cx| this.remove_browser(cx))),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(tokens.text_secondary)
                        .child("Chrome for Testing is shared and is not removed."),
                )
            })
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
            State::Idle | State::Current | State::External(_) | State::Failed(_) => {
                self.check_for_updates(cx);
            }
        }
    }

    fn render_updates(&self, cx: &mut Context<Self>) -> gpui::Div {
        use gpui_component::button::ButtonVariants as _;
        use updater::State;
        let tokens = self.tokens(cx);
        let (message, action) = match &self.update_state {
            State::Idle => (
                "Check GitHub for a new version.".into(),
                Some("Check for updates"),
            ),
            State::Checking => ("Checking for updates…".into(), None),
            State::Current => ("Shob is up to date.".into(), Some("Check for updates")),
            State::Available(update) => (
                format!("Shob {} is available.", update.version()),
                Some("Update"),
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
                    .child(concat!("Installed version: ", env!("CARGO_PKG_VERSION"))),
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
                        Button::new("shob-update-action")
                            .label(label)
                            .primary()
                            .on_click(
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

    fn render_restore_section(&self, cx: &mut Context<Self>) -> gpui::Div {
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
                    .debug_selector(move || id.into())
                    .tab_index(0)
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

        div()
            .rounded(px(8.0))
            .border_1()
            .border_color(tokens.card_border)
            .bg(tokens.card_bg)
            .overflow_hidden()
            .children(toggle_rows)
    }

    fn render_archive_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let archived_threads = self.sidebar_archived_thread_refs();
        let empty = archived_threads.is_empty();
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
                            .tab_index(0)
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
        div()
            .debug_selector(|| "archived-threads".into())
            .rounded(px(8.0))
            .border_1()
            .border_color(tokens.card_border)
            .bg(tokens.card_bg)
            .overflow_hidden()
            .when(empty, |card| {
                card.child(
                    div()
                        .p(px(16.0))
                        .text_size(px(13.0))
                        .text_color(tokens.text_secondary)
                        .child("No archived threads yet."),
                )
            })
            .children(archived_rows)
    }

    fn render_storage_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let confirm = self.clear_data_confirm;
        let data_dir = paths::data_dir();
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
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
                                    .truncate()
                                    .child(data_dir.display().to_string()),
                            ),
                    )
                    .child(
                        div()
                            .id("open-data-folder")
                            .debug_selector(|| "open-data-folder".into())
                            .tab_index(0)
                            .flex_shrink_0()
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
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .p(px(14.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(tokens.block_failure.opacity(0.4))
                    .bg(tokens.card_bg)
                    .child(div().text_size(px(13.0)).font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(tokens.text_primary).child("Clear saved sessions & history"))
                    .child(div().text_size(px(12.0)).text_color(tokens.text_secondary)
                        .child(if confirm {
                            "This cannot be undone. Your settings will be kept."
                        } else {
                            "Delete saved layouts, scrollback, and Shob-owned agent sessions. Your settings will be kept."
                        }))
                    .child(
                        div().flex().flex_wrap().gap(px(8.0))
                            .when(confirm, |actions| actions.child(
                                div().id("cancel-clear-saved-data").debug_selector(|| "cancel-clear-saved-data".into())
                                    .tab_index(0).px(px(10.0)).py(px(6.0)).rounded(px(6.0))
                                    .bg(tokens.secondary_btn_bg).text_size(px(12.0)).text_color(tokens.text_primary)
                                    .cursor_pointer().hover(move |button| button.bg(tokens.secondary_btn_hover))
                                    .child("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.clear_data_confirm = false;
                                        this.settings_focus.focus(window, cx);
                                        cx.notify();
                                    })),
                            ))
                            .child(
                                div().id(if confirm { "confirm-clear-saved-data" } else { "clear-saved-data" })
                                    .debug_selector(move || if confirm { "confirm-clear-saved-data" } else { "clear-saved-data" }.into())
                                    .tab_index(0).px(px(10.0)).py(px(6.0)).rounded(px(6.0))
                                    .bg(tokens.block_failure.opacity(0.12)).text_color(tokens.block_failure).text_size(px(12.0))
                                    .cursor_pointer().hover(move |button| button.bg(tokens.block_failure.opacity(0.20)))
                                    .child(if confirm { "Confirm clear" } else { "Clear saved data…" })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.settings_focus.focus(window, cx);
                                        this.clear_saved_data(cx);
                                    })),
                            ),
                    ),
            )
    }
}
