impl AppState {
    fn toggle_restore_option(&mut self, id: &'static str, cx: &mut Context<Self>) {
        match id {
            "restore-sessions" => self.settings.restore.sessions = !self.settings.restore.sessions,
            "restore-scrollback" => {
                self.settings.restore.scrollback = !self.settings.restore.scrollback
            }
            "restore-agent-history" => {
                self.settings.restore.agent_history = !self.settings.restore.agent_history
            }
            _ => return,
        }
        self.clear_data_confirm = false;
        self.save_generation.fetch_add(1, Ordering::SeqCst);
        if let Err(error) = settings::save_preserving_theme(&self.saved_settings()) {
            eprintln!("failed to save settings: {error}");
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn open_data_folder(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = paths::open_in_file_manager(&paths::data_dir()) {
            eprintln!("failed to open the data folder: {error}");
        }
        cx.notify();
    }

    fn clear_saved_data(&mut self, cx: &mut Context<Self>) {
        if !self.clear_data_confirm {
            self.clear_data_confirm = true;
            cx.notify();
            return;
        }
        self.clear_data_confirm = false;
        let mut records: HashMap<String, workspace_state::PendingDeletion> = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(paths::workspace_path()) {
            let saved = workspace_state::load_from_text(&text);
            for session in saved
                .projects
                .into_iter()
                .flat_map(|project| project.sessions)
            {
                let record = records.entry(session.uid.clone()).or_insert_with(|| {
                    workspace_state::PendingDeletion {
                        session_uid: session.uid,
                        pane_uids: Vec::new(),
                        shob_session_ids: Vec::new(),
                    }
                });
                for pane in session.panes {
                    record.pane_uids.push(pane.uid);
                    record.shob_session_ids.extend(pane.chat.shob_session_id);
                    record.shob_session_ids.extend(
                        pane.chat
                            .models
                            .into_iter()
                            .filter_map(|model| model.shob_session_id),
                    );
                }
            }
        }
        for session in self.projects.iter().flat_map(|project| &project.sessions) {
            let record = records.entry(session.uid.clone()).or_insert_with(|| {
                workspace_state::PendingDeletion {
                    session_uid: session.uid.clone(),
                    pane_uids: Vec::new(),
                    shob_session_ids: Vec::new(),
                }
            });
            for pane in &session.panes {
                record.pane_uids.push(pane.uid.clone());
                record
                    .shob_session_ids
                    .extend(pane.chat.shob_session_id.clone());
            }
        }
        let mut deletions = Vec::new();
        for deletion in records.into_values() {
            match workspace_state::record_deletion(&deletion) {
                Ok(path) => deletions.push((path, deletion)),
                Err(error) => {
                    for (path, _) in &deletions {
                        let _ = std::fs::remove_file(path);
                    }
                    eprintln!("failed to record saved data deletion: {error}");
                    cx.notify();
                    return;
                }
            }
        }
        for pane in self
            .projects
            .iter_mut()
            .flat_map(|project| &mut project.sessions)
            .flat_map(|session| &mut session.panes)
        {
            pane.chat.bridge = None;
            pane.chat.bridge_resume = None;
            pane.chat.shob_session_id = None;
        }
        if let Err(error) = std::fs::remove_file(paths::workspace_path())
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("failed to remove saved workspace: {error}");
            cx.notify();
            return;
        }
        let _ = std::fs::remove_dir_all(paths::sessions_dir());
        let _ = std::fs::create_dir_all(paths::sessions_dir());
        if let Err(error) = conversations::clear_all() {
            eprintln!("failed to clear conversations: {error}");
        }
        for (path, deletion) in deletions {
            if let Err(error) = workspace_state::finish_deletion(&path, &deletion) {
                eprintln!("failed to clear SHOB session; cleanup will retry: {error}");
            }
        }
        self.dirty = false;
        cx.notify();
    }

    /// Palette cards show both appearances of each built-in theme.
    fn render_palette_scheme_picker(&self, narrow: bool, cx: &mut Context<Self>) -> gpui::Div {
        let tokens = self.tokens(cx);
        let cards = PaletteScheme::ALL.into_iter().map(|scheme| {
            let selected = self.palette_scheme == scheme;
            let swatch = |mode| {
                let preview = ThemeTokens::from_mode_with_scheme(mode, scheme);
                div()
                    .size(px(36.0))
                    .rounded_full()
                    .border_1()
                    .border_color(if selected {
                        tokens.ring
                    } else {
                        tokens.card_border
                    })
                    .bg(gpui::linear_gradient(
                        135.0,
                        gpui::linear_color_stop(preview.primary_btn_bg, 0.0),
                        gpui::linear_color_stop(preview.pane_bg, 1.0),
                    ))
            };
            div()
                .id(("palette-scheme", scheme as usize))
                .debug_selector(move || scheme.label().to_owned())
                .tab_index(0)
                .min_w_0()
                .h(px(104.0))
                .p(px(12.0))
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
                .focus_visible(move |style| style.border_color(tokens.ring))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_palette_scheme(scheme, Some(window), cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(10.0))
                        .child(swatch(gpui_component::ThemeMode::Light))
                        .child(swatch(gpui_component::ThemeMode::Dark)),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(tokens.text_primary)
                        .child(scheme.label()),
                )
                .flex()
                .flex_col()
                .into_any_element()
        });
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .text_size(px(16.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(tokens.text_primary)
                    .child("Built-in palettes"),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(if narrow { 2 } else { 3 })
                    .gap(px(10.0))
                    .children(cards),
            )
    }

    fn render_integrations_panel(&self, cx: &mut Context<Self>) -> gpui::Div {
        let weak = cx.entity().downgrade();
        let tokens = self.tokens(cx);
        div()
            .w_full()
            .px(px(12.0))
            .py(px(10.0))
            .border_t_1()
            .border_color(tokens.sidebar_border)
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(div().text_size(px(12.0)).font_weight(gpui::FontWeight::BOLD).text_color(tokens.sidebar_fg).child("Agent integrations"))
            .child(div().text_size(px(10.5)).text_color(tokens.sidebar_muted).child("Claude Code · Codex · OpenCode · Pi · OMP · Command Code"))
            .child(div().text_size(px(10.0)).text_color(tokens.sidebar_muted).child("Versions and hook trust: shob integration doctor"))
            .child(div().text_size(px(10.0)).text_color(tokens.sidebar_muted).child(self.integration_feedback.clone()))
            .child(
                div()
                    .id("enable-agent-integrations")
                    .w_full().py(px(6.0)).rounded(px(6.0))
                    .bg(tokens.card_bg).hover(move |s| s.bg(tokens.card_hover))
                    .cursor(CursorStyle::PointingHand)
                    .text_size(px(11.0)).text_color(tokens.sidebar_fg)
                    .flex().items_center().justify_center()
                    .child("Enable local integrations")
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        let target = weak.clone();
                        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                            let result = cx.background_executor().spawn(async move {
                                std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| {
                                    std::process::Command::new(exe).args(["integration", "setup"]).output().map_err(anyhow::Error::from)
                                })
                            }).await;
                            let message = match result {
                                Ok(output) if output.status.success() => "Installed. Restart each agent; Codex may also ask you to trust its hooks.".to_string(),
                                Ok(output) => format!("Setup failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
                                Err(error) => format!("Setup failed: {error}"),
                            };
                            let _ = target.update(cx, |this, cx| { this.integration_feedback = SharedString::from(message); cx.notify(); });
                        }).detach();
                    }),
            )
    }

    #[cfg(test)]
    fn reorder_project(&mut self, source_id: usize, target_id: usize, cx: &mut Context<Self>) {
        let Some(source) = self
            .projects
            .iter()
            .position(|project| project.id == source_id)
        else {
            return;
        };
        let Some(target) = self
            .projects
            .iter()
            .position(|project| project.id == target_id)
        else {
            return;
        };
        if source == target {
            return;
        }
        let active_id = self.active_project().map(|project| project.id);
        let project = self.projects.remove(source);
        self.projects.insert(target, project);
        if let Some(index) = self
            .projects
            .iter()
            .position(|project| Some(project.id) == active_id)
        {
            self.active_project_idx = index;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn render_empty_terminal(&self, cx: &mut Context<Self>) -> gpui::Div {
        let has_project = self.active_project().is_some();
        let project_idx = self.active_project_idx;
        let tokens = self.tokens(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .child(
                div()
                    .size(px(48.0))
                    .rounded(px(12.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(tokens.card_bg)
                    .child(
                        svg()
                            .path("icons/folder.svg")
                            .size(px(24.0))
                            .text_color(tokens.text_secondary),
                    ),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(tokens.text_primary)
                    .child(if has_project {
                        "No session in this project"
                    } else {
                        "No project open"
                    }),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(tokens.text_muted)
                    .child(if has_project {
                        "Create a session to start a terminal in this project."
                    } else {
                        "Open a folder to start a project with its own sessions."
                    }),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(sidebar_text_button(
                        "empty-terminal-open-project",
                        "Open project",
                        "icons/folder-plus.svg",
                        true,
                        &tokens,
                        cx.listener::<MouseDownEvent>(|this, _, window, cx| {
                            this.select_folder_and_add_project(window, cx);
                        }),
                    ))
                    .when(has_project, |this| {
                        this.child(sidebar_text_button(
                            "empty-terminal-new-session",
                            "New session",
                            "icons/plus.svg",
                            false,
                            &tokens,
                            cx.listener::<MouseDownEvent>(move |this, _, window, cx| {
                                this.create_new_session_in_project(project_idx, window, cx);
                            }),
                        ))
                    }),
            )
    }

    /// Switch the workspace to a session in a given project.
    fn select_session(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_view = ActiveView::Workspace;
        self.compact_sidebar_open = false;
        if !self
            .projects
            .get(project_idx)
            .is_some_and(|project| session_idx < project.sessions.len())
        {
            return;
        }
        self.clear_sidebar_search(window, cx);
        let project = &mut self.projects[project_idx];
        project.active_session_idx = session_idx;
        project.expanded = true;
        self.active_project_idx = project_idx;
        let pane_id = project.sessions[session_idx].panes.first().map(|p| p.id);
        if let Some(pane_id) = pane_id {
            self.focus_workspace_pane(pane_id, window, cx);
        } else {
            self.focused_pane_id = None;
        }
        self.schedule_workspace_save(cx);
        cx.notify();
    }
}
