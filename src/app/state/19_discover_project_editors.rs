impl AppState {
    fn discover_project_editors() -> Vec<(String, PathBuf)> {
        let paths = std::env::var_os("PATH").unwrap_or_default();
        [
            ("Zed", "zed"),
            ("VS Code", "code"),
            ("VS Code Insiders", "code-insiders"),
            ("VSCodium", "codium"),
            ("Cursor", "cursor"),
            ("Trae", "trae"),
            ("Kiro", "kiro"),
            ("Antigravity", "antigravity-ide"),
            ("IntelliJ IDEA", "idea"),
            ("Aqua", "aqua"),
            ("CLion", "clion"),
            ("DataGrip", "datagrip"),
            ("DataSpell", "dataspell"),
            ("GoLand", "goland"),
            ("PhpStorm", "phpstorm"),
            ("PyCharm", "pycharm"),
            ("Rider", "rider"),
            ("RubyMine", "rubymine"),
            ("RustRover", "rustrover"),
            ("WebStorm", "webstorm"),
        ]
        .into_iter()
        .filter_map(|(name, command)| {
            std::env::split_paths(&paths)
                .map(|dir| {
                    dir.join(if cfg!(windows) {
                        format!("{command}.exe")
                    } else {
                        command.to_owned()
                    })
                })
                .find(|path| path.is_file())
                .map(|path| (name.to_owned(), path))
        })
        .collect()
    }

    fn render_project_editor(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pane_id = pane.id;
        let preferred = Self::preferred_project_editor(
            &self.available_editors,
            self.settings.preferred_editor.as_deref(),
        )
        .map(|(name, _)| name.as_str());
        let open = self.editor_picker_open == Some(pane_id);
        let hover = tokens.pane_header_btn_hover;
        div()
            .child(
                div()
                    .flex()
                    .h(px(32.0))
                    .items_center()
                    .gap(px(2.0))
                    .child(
                        div()
                            .id(("open-default-editor", pane_id))
                            .debug_selector(|| "open-default-editor".into())
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .px(px(6.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .child(project_editor_icon(preferred, tokens.text_secondary))
                            .child({
                                let label = preferred.map_or_else(
                                    || "Open in editor".to_owned(),
                                    |name| format!("Open in {name}"),
                                );
                                div()
                                    .id("default-editor-label")
                                    .debug_selector({
                                        let label = label.clone();
                                        move || label.clone()
                                    })
                                    .truncate()
                                    .text_size(px(13.0))
                                    .child(label)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_project_editor(pane_id, None, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .id(("open-editor-picker", pane_id))
                            .debug_selector(|| "open-editor-picker".into())
                            .size(px(30.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .tooltip(|window, cx| {
                                gpui_component::tooltip::Tooltip::new("Choose editor")
                                    .build(window, cx)
                            })
                            .child(
                                svg()
                                    .path(if open {
                                        "ui_icons/chevron-up.svg"
                                    } else {
                                        "icons/chevron-down.svg"
                                    })
                                    .size(px(12.0))
                                    .text_color(tokens.text_secondary),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.editor_picker_open =
                                    (this.editor_picker_open != Some(pane_id)).then_some(pane_id);
                                if this.editor_picker_open.is_some() {
                                    this.refresh_project_editors(cx);
                                }
                                cx.notify();
                            })),
                    ),
            )
            .when(open, |menu| {
                menu.child(
                    div()
                        .mx(px(4.0))
                        .p(px(4.0))
                        .rounded(px(7.0))
                        .border_1()
                        .border_color(tokens.border_subtle)
                        .bg(tokens.card_bg)
                        .when(self.available_editors.is_empty(), |rows| {
                            rows.child(
                                div()
                                    .p(px(6.0))
                                    .text_size(px(12.0))
                                    .text_color(tokens.text_muted)
                                    .child("No editors found on PATH"),
                            )
                        })
                        .children(self.available_editors.iter().enumerate().map(
                            |(index, (name, _))| {
                                let name = name.clone();
                                div()
                                    .id(("open-editor", index))
                                    .h(px(30.0))
                                    .px(px(6.0))
                                    .rounded(px(5.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .cursor_pointer()
                                    .hover(move |style| style.bg(hover))
                                    .text_size(px(12.0))
                                    .child(project_editor_icon(Some(&name), tokens.text_secondary))
                                    .child(div().flex_1().child(name.clone()))
                                    .when(preferred == Some(name.as_str()), |row| {
                                        row.child(
                                            svg()
                                                .path("icons/status-check.svg")
                                                .size(px(12.0))
                                                .text_color(tokens.text_secondary),
                                        )
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_project_editor(
                                            pane_id,
                                            Some(name.clone()),
                                            window,
                                            cx,
                                        )
                                    }))
                            },
                        )),
                )
            })
    }

    fn desktop_titlebar(&self, cx: &Context<Self>) -> TitleBar {
        let state = cx.weak_entity();
        TitleBar::new().on_close_window(move |_, window, cx| {
            let _ = state.update(cx, |this, cx| this.save_all_sync(Some(window), cx));
            window.remove_window();
        })
    }

    fn render_focused_agent_info(
        &self,
        pane: &TerminalPane,
        has_usage: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let tokens = self.tokens(cx);
        let pane_id = pane.id;
        let active_dir = self.agent_working_directory(pane);
        let branch = Self::detect_git_branch(&active_dir)
            .or_else(|| self.pane_git_branch(pane))
            .unwrap_or_else(|| "Not in a Git repository".into());
        let summary = self
            .project_change_counts
            .get(&active_dir)
            .or_else(|| {
                self.pane_index
                    .get(&pane_id)
                    .and_then(|&(project, _, _)| self.projects.get(project))
                    .and_then(|project| self.project_change_counts.get(&project.path))
            })
            .and_then(Option::as_ref)
            .filter(|summary| summary.files > 0);
        let owner = self
            .pane_index
            .get(&pane_id)
            .and_then(|&(project, _, _)| self.projects.get(project));
        let project_path = owner.map(|project| project.path.clone());
        let scripts = project_path
            .as_ref()
            .and_then(|path| self.settings.project_scripts.get(path));
        let project = owner
            .map(|project| project.name.clone())
            .unwrap_or_else(|| SharedString::from("Environment"));
        div()
            .id(("focused-agent-info", pane_id))
            .debug_selector(|| "focused-agent-info".into())
            .absolute()
            .top(if has_usage { px(70.0) } else { px(46.0) })
            .right(px(16.0))
            .w(px(280.0))
            .max_w(relative(0.9))
            .rounded(px(14.0))
            .border_1()
            .border_color(tokens.card_border)
            .bg(tokens.card_hover)
            .shadow_md()
            .overflow_hidden()
            .child(
                div()
                    .id(("project-details-body", pane_id))
                    .max_h(px(520.0))
                    .overflow_y_scrollbar()
                    .flex()
                    .flex_col()
                    .p(px(8.0))
                    .gap(px(2.0))
                    .child(
                        div()
                            .h(px(30.0))
                            .px(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                svg()
                                    .path("icons/monitor.svg")
                                    .size(px(15.0))
                                    .text_color(tokens.text_secondary),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(13.0))
                                    .text_color(tokens.text_secondary)
                                    .truncate()
                                    .child(project),
                            )
                            .child(
                                div()
                                    .id(("project-folder", pane_id))
                                    .cursor_pointer()
                                    .child(
                                        svg()
                                            .path("icons/folder.svg")
                                            .size(px(15.0))
                                            .text_color(tokens.text_secondary),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            if let Some(&(project, _, _)) =
                                                this.pane_index.get(&pane_id)
                                            {
                                                let path = this.projects[project].path.clone();
                                                cx.background_executor()
                                                    .spawn(async move {
                                                        let _ = paths::open_in_file_manager(&path);
                                                    })
                                                    .detach();
                                            }
                                        }),
                                    ),
                            ),
                    )
                    .child(self.render_project_editor(pane, &tokens, cx))
                    .children(scripts.into_iter().flatten().take(32).enumerate().map(
                        |(index, script)| {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .px(px(6.0))
                                .h(px(30.0))
                                .child(
                                    div()
                                        .id(("run-project-script", index))
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .cursor_pointer()
                                        .text_size(px(12.0))
                                        .child(format!("▶  {}", script.name))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.run_project_script(pane_id, index, window, cx);
                                            }),
                                        ),
                                )
                                .child(
                                    div()
                                        .id(("remove-project-script", index))
                                        .cursor_pointer()
                                        .text_color(tokens.text_muted)
                                        .child("×")
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                if let Some(&(project, _, _)) =
                                                    this.pane_index.get(&pane_id)
                                                    && let Some(scripts) = this
                                                        .settings
                                                        .project_scripts
                                                        .get_mut(&this.projects[project].path)
                                                    && index < scripts.len()
                                                {
                                                    scripts.remove(index);
                                                }
                                                this.schedule_workspace_save(cx);
                                                cx.notify();
                                            }),
                                        ),
                                )
                        },
                    ))
                    .child(
                        div()
                            .id(("add-project-script", pane_id))
                            .h(px(30.0))
                            .px(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .cursor_pointer()
                            .text_color(tokens.text_secondary)
                            .text_size(px(12.0))
                            .child("+   Add project script")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_project_script_form(pane_id, window, cx);
                                }),
                            ),
                    )
                    .when_some(
                        self.project_script_form
                            .as_ref()
                            .filter(|(id, _, _)| *id == pane_id),
                        |view, (_, name, command)| {
                            view.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.0))
                                    .py(px(6.0))
                                    .child(Input::new(name).appearance(true).bordered(true))
                                    .child(Input::new(command).appearance(true).bordered(true))
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(tokens.text_muted)
                                            .child("Runs in a new terminal tab in this project."),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap(px(12.0))
                                            .child(
                                                div()
                                                    .id(("save-project-script", pane_id))
                                                    .cursor_pointer()
                                                    .child("Save")
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(|this, _, _, cx| {
                                                            cx.stop_propagation();
                                                            this.save_project_script(cx);
                                                        }),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .id(("cancel-project-script", pane_id))
                                                    .cursor_pointer()
                                                    .child("Cancel")
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(|this, _, _, cx| {
                                                            cx.stop_propagation();
                                                            this.project_script_form = None;
                                                            cx.notify();
                                                        }),
                                                    ),
                                            ),
                                    ),
                            )
                        },
                    )
                    .child(
                        div()
                            .mt(px(5.0))
                            .pt(px(5.0))
                            .border_t_1()
                            .border_color(tokens.border_subtle)
                            .child(
                                div()
                                    .h(px(30.0))
                                    .px(px(6.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .child(
                                        svg()
                                            .path("icons/git-branch.svg")
                                            .size(px(14.0))
                                            .text_color(tokens.text_secondary),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_size(px(12.0))
                                            .text_color(tokens.text_primary)
                                            .truncate()
                                            .child(branch.clone()),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id(("project-commit", pane_id))
                            .h(px(30.0))
                            .px(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(tokens.pane_header_btn_hover))
                            .text_size(px(12.0))
                            .child(
                                svg()
                                    .path("icons/git-branch.svg")
                                    .size(px(14.0))
                                    .text_color(tokens.text_secondary),
                            )
                            .child(div().flex_1().child("Commit"))
                            .child(
                                svg()
                                    .path("icons/chevron-right.svg")
                                    .size(px(12.0))
                                    .text_color(tokens.text_secondary),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_files_surface(pane_id, window, cx);
                                    if let Some(view) = this
                                        .focused_pane_id
                                        .and_then(|id| this.pane_ref(id))
                                        .and_then(|p| p.files_surface.as_ref())
                                        .map(|(view, _)| view.clone())
                                    {
                                        view.update(cx, |view, cx| view.show_git(cx));
                                    }
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id(("agent-changes", pane_id))
                            .debug_selector(|| "agent-changes".into())
                            .h(px(30.0))
                            .px(px(6.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(tokens.pane_header_btn_hover))
                            .child(
                                svg()
                                    .path("icons/status-check.svg")
                                    .size(px(14.0))
                                    .text_color(tokens.text_secondary),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(tokens.text_primary)
                                    .child("Changes"),
                            )
                            .child(div().flex_1())
                            .when_some(summary, |row, summary| {
                                row.child(
                                    div()
                                        .debug_selector(|| "agent-changes-added".into())
                                        .text_size(px(11.0))
                                        .text_color(if tokens.is_dark {
                                            rgba(0x3fb950ff)
                                        } else {
                                            rgba(0x1a7f37ff)
                                        })
                                        .child(format!("+{}", summary.additions)),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| "agent-changes-removed".into())
                                        .text_size(px(11.0))
                                        .text_color(if tokens.is_dark {
                                            rgba(0xf85149ff)
                                        } else {
                                            rgba(0xcf222eff)
                                        })
                                        .child(format!("-{}", summary.deletions)),
                                )
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_project_diff(pane_id, window, cx);
                                }),
                            ),
                    ),
            )
    }

    fn render_tool_panel_chooser(
        &self,
        owner: usize,
        focus: &gpui::FocusHandle,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tokens = self.tokens(cx);
        let choices = [
            (
                "tool-panel-choose-terminal",
                "Terminal",
                "icons/terminal.svg",
                "T",
                Self::new_tool_panel_terminal
                    as fn(&mut Self, usize, &mut Window, &mut Context<Self>),
            ),
            (
                "tool-panel-choose-files",
                "Files",
                "ui_icons/file.svg",
                "F",
                Self::open_files_surface,
            ),
            (
                "tool-panel-choose-diff",
                "Diff",
                "icons/git-branch.svg",
                "D",
                Self::open_project_diff,
            ),
        ];
        let click_focus = focus.clone();
        div()
            .id(("tool-panel-chooser", owner))
            .debug_selector(|| "tool-panel-chooser".into())
            .track_focus(focus)
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                click_focus.focus(window, cx)
            })
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers != gpui::Modifiers::none() {
                    return;
                }
                let key = event.keystroke.key.as_str();
                if key == "escape" {
                    cx.stop_propagation();
                    this.dismiss_tool_panel_chooser(owner, window, cx);
                } else if let Some((_, _, _, _, open)) = choices
                    .iter()
                    .find(|(_, _, _, shortcut, _)| key.eq_ignore_ascii_case(shortcut))
                {
                    cx.stop_propagation();
                    open(this, owner, window, cx);
                }
            }))
            .child(
                div()
                    .w(px(310.0))
                    .max_w_full()
                    .px(px(8.0))
                    .text_size(px(14.0))
                    .text_color(tokens.text_primary)
                    .child(div().pb(px(12.0)).text_center().child("Open a surface"))
                    .children(choices.map(|(selector, label, icon, key, open)| {
                        div()
                            .id(selector)
                            .debug_selector(move || selector.into())
                            .h(px(34.0))
                            .px(px(8.0))
                            .rounded(px(5.0))
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .cursor_pointer()
                            .hover(|row| row.bg(tokens.card_hover))
                            .child(
                                svg()
                                    .path(icon)
                                    .size(px(14.0))
                                    .text_color(tokens.text_secondary)
                                    .flex_shrink_0(),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            .child(
                                div()
                                    .size(px(20.0))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .bg(tokens.card_hover)
                                    .text_color(tokens.text_secondary)
                                    .text_size(px(11.0))
                                    .child(key),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    open(this, owner, window, cx);
                                }),
                            )
                    })),
            )
    }

    fn render_tool_panel_tabs(
        &self,
        owner: &TerminalPane,
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tokens = self.tokens(cx);
        let owner_id = owner.id;
        let has_tabs = self.tool_panel_panes(owner_id).next().is_some();
        let tabs = div()
            .id(("tool-panel-tabs", owner_id))
            .debug_selector(|| "tool-panel-tabs".into())
            .flex()
            .min_w_0()
            .gap(px(4.0))
            .overflow_x_scroll()
            .when_some(owner.tool_panel_scroll.as_ref(), |row, handle| {
                row.track_scroll(handle)
            })
            .children(self.tool_panel_panes(owner_id).map(|pane| {
                let id = pane.id;
                let selected = Some(id) == active;
                let icon = if pane.files_active
                    || pane.diff_preview.is_some()
                    || pane.markdown_preview.is_some()
                {
                    "ui_icons/file.svg"
                } else {
                    "icons/terminal.svg"
                };
                let foreground = if selected {
                    tokens.text_primary
                } else {
                    tokens.text_secondary
                };
                div()
                    .id(("tool-panel-tab", id))
                    .h(px(24.0))
                    .max_w(px(168.0))
                    .min_w(px(64.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .pl(px(8.0))
                    .pr(px(3.0))
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .text_color(foreground)
                    .when(selected, |tab| tab.bg(tokens.card_hover))
                    .hover(|tab| tab.bg(tokens.card_hover))
                    .child(
                        svg()
                            .path(icon)
                            .size(px(13.0))
                            .text_color(foreground)
                            .flex_shrink_0(),
                    )
                    .child(div().min_w_0().truncate().child(pane.display_title.clone()))
                    .child(
                        sidebar_icon_button(
                            ("close-tool-panel-tab", id),
                            "icons/x.svg",
                            11.0,
                            &tokens,
                            cx.listener(move |this, _, window, cx| this.close_pane(id, window, cx)),
                        )
                        .tooltip(|window, cx| {
                            gpui_component::tooltip::Tooltip::new("Close tab").build(window, cx)
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.select_tool_panel_tab(owner_id, id, window, cx);
                        }),
                    )
            }));

        div()
            .h(px(36.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(7.0))
            .when(has_tabs, |bar| {
                bar.border_b_1().border_color(tokens.border_subtle)
            })
            .bg(tokens.pane_bg)
            .child(tabs)
            .when(has_tabs, |row| {
                row.child(
                    sidebar_icon_button(
                        ("add-tool-panel-surface", owner_id),
                        "icons/plus.svg",
                        13.0,
                        &tokens,
                        cx.listener(move |this, _, window, cx| {
                            this.show_tool_panel_chooser(owner_id, window, cx)
                        }),
                    )
                    .debug_selector(|| "add-tool-panel-surface".into())
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("Open a surface").build(window, cx)
                    }),
                )
            })
            .child(div().flex_1())
            .child(
                sidebar_icon_button(
                    ("hide-tool-panel", owner_id),
                    "icons/panel-right.svg",
                    14.0,
                    &tokens,
                    cx.listener(move |this, _, window, cx| {
                        this.hide_tool_panel(owner_id, window, cx)
                    }),
                )
                .debug_selector(|| "hide-tool-panel".into())
                .tooltip(|window, cx| {
                    gpui_component::tooltip::Tooltip::new("Hide right panel").build(window, cx)
                }),
            )
    }
}
