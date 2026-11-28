use super::*;

impl Render for FilesSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(owner) = self.owner.upgrade() else {
            return gpui::Empty.into_any_element();
        };
        let tokens = owner.read(cx).tokens(cx);
        let weak = cx.entity().downgrade();
        let count = if self.git {
            self.repository.as_ref().map_or(0, |r| r.changes.len())
        } else {
            self.entries.len()
        };
        let rows = uniform_list("files-tree", count, move |range, _, cx| {
            let Some(entity) = weak.upgrade() else {
                return Vec::new();
            };
            let this = entity.read(cx);
            range
                .filter_map(|index| {
                    let (path, directory, depth, status, staged, unstaged) = if this.git {
                        let c = this.repository.as_ref()?.changes.get(index)?;
                        (
                            c.path.clone(),
                            false,
                            0,
                            c.status.clone(),
                            c.staged,
                            c.unstaged,
                        )
                    } else {
                        let e = this.entries.get(index)?;
                        let status = this
                            .repository
                            .as_ref()
                            .and_then(|r| r.statuses.get(&this.root.join(&e.path)))
                            .cloned()
                            .unwrap_or_default();
                        (e.path.clone(), e.directory, e.depth, status, false, false)
                    };
                    let change_color = if status.contains('D') {
                        tokens.block_failure
                    } else if status.contains('A') || status == "??" {
                        tokens.block_success
                    } else if status.trim().is_empty() {
                        tokens.text_primary
                    } else {
                        tokens.block_warning
                    };
                    let expanded = this.expanded.contains(&path);
                    let label = if this.git {
                        path.to_string_lossy().into_owned()
                    } else {
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    };
                    let selected = this.selected.as_ref() == Some(&path) && !this.git;
                    let click_path = path.clone();
                    let git_mode = this.git;
                    let row_entity = entity.clone();
                    let mut row = div()
                        .id(("file-row", index))
                        .h(px(26.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(8.0 + depth as f32 * 16.0))
                        .pr(px(8.0))
                        .text_size(px(12.0))
                        .text_color(tokens.text_primary)
                        .when(selected, |s| s.bg(tokens.card_hover))
                        .cursor_pointer()
                        .hover(|s| s.bg(tokens.card_hover))
                        .child(
                            svg()
                                .path(if directory {
                                    if expanded {
                                        "zed_file_icons/chevron_down.svg"
                                    } else {
                                        "zed_file_icons/chevron_right.svg"
                                    }
                                } else {
                                    "zed_file_icons/chevron_right.svg"
                                })
                                .when(!directory, |s| s.opacity(0.0))
                                .size(px(12.0))
                                .text_color(tokens.text_muted),
                        )
                        .child(
                            svg()
                                .path(if directory {
                                    if expanded {
                                        "zed_file_icons/folder_open.svg".to_owned()
                                    } else {
                                        "zed_file_icons/folder.svg".to_owned()
                                    }
                                } else {
                                    crate::file_icons::icon_for_path(&path.to_string_lossy())
                                        .to_string()
                                })
                                .size(px(16.0))
                                .flex_shrink_0()
                                .text_color(tokens.text_secondary),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(change_color)
                                .child(label),
                        )
                        .child(div().text_color(change_color).child(status))
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            cx.stop_propagation();
                            row_entity.update(cx, |this, cx| {
                                if directory {
                                    if !this.expanded.remove(&click_path) {
                                        if this.expanded.len() >= MAX_ENTRIES || click_path.to_str().is_none() {
                                            this.message = "Folder expansion limit reached or folder name is not UTF-8.".into();
                                            cx.notify();
                                            return;
                                        }
                                        this.expanded.insert(click_path.clone());
                                    }
                                    this.refresh(cx);
                                    this.changed(cx);
                                } else if git_mode {
                                    if let Some(change) = this
                                        .repository
                                        .as_ref()
                                        .and_then(|r| {
                                            r.changes.iter().find(|c| c.path == click_path)
                                        })
                                        .cloned()
                                    {
                                        this.show_diff(change, window, cx);
                                    }
                                } else {
                                    this.open(
                                        SavedFile {
                                            path: click_path.clone(),
                                            ..Default::default()
                                        },
                                        true,
                                        window,
                                        cx,
                                    );
                                }
                            });
                        });
                    for (enabled, label, stage) in [(unstaged, "+", true), (staged, "−", false)] {
                        if !enabled {
                            continue;
                        }
                        let action_path = path.clone();
                        let action_entity = entity.clone();
                        row = row.child(
                            div()
                                .id((if stage { "stage" } else { "unstage" }, index))
                                .px(px(5.0))
                                .cursor_pointer()
                                .child(label)
                                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                    cx.stop_propagation();
                                    action_entity.update(cx, |this, cx| {
                                        if stage {
                                            this.git_action(
                                                vec![
                                                    "add".into(),
                                                    "--".into(),
                                                    action_path.clone().into_os_string(),
                                                ],
                                                cx,
                                            );
                                        } else {
                                            this.unstage(action_path.clone(), cx);
                                        }
                                    });
                                }),
                        );
                    }
                    Some(row)
                })
                .collect()
        })
        .flex_1()
        .min_h_0();
        let mut sidebar = div()
            .w(px(260.0))
            .min_w(px(180.0))
            .max_w(relative(0.45))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(tokens.border_subtle)
            .child(
                div()
                    .h(px(34.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .px(px(10.0))
                    .child(
                        div()
                            .id("browse-files")
                            .cursor_pointer()
                            .text_color(if self.git {
                                tokens.text_muted
                            } else {
                                tokens.text_primary
                            })
                            .child("Project")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.git = false;
                                    this.changed(cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("browse-git")
                            .cursor_pointer()
                            .text_color(if self.git {
                                tokens.text_primary
                            } else {
                                tokens.text_muted
                            })
                            .child(format!(
                                "Git ({})",
                                self.repository.as_ref().map_or(0, |r| r.changes.len())
                            ))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.git = true;
                                    this.refresh(cx);
                                    this.changed(cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("refresh-files")
                            .cursor_pointer()
                            .child("↻")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.refresh(cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .h(px(28.0))
                    .px(px(10.0))
                    .text_size(px(11.0))
                    .text_color(tokens.text_secondary)
                    .truncate()
                    .child(if self.git {
                        self.repository
                            .as_ref()
                            .map_or("No Git repository".into(), |r| r.branch.clone())
                    } else {
                        self.root
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }),
            )
            .child(rows);
        if self.git && self.repository.is_some() {
            sidebar = sidebar.child(
                div()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(Input::new(&self.commit))
                    .child(
                        div()
                            .id("commit-files")
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .child(if self.operation.is_some() {
                                "Working…"
                            } else {
                                "Commit staged changes"
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    let message = this.commit.read(cx).value().to_string();
                                    if message.trim().is_empty() {
                                        this.message = "Enter a commit message first.".into();
                                        cx.notify();
                                        return;
                                    }
                                    this.git_action(
                                        vec!["commit".into(), "-m".into(), message.into()],
                                        cx,
                                    );
                                }),
                            ),
                    ),
            );
        }
        let mut tabs = div()
            .h(px(34.0))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .overflow_hidden()
            .border_b_1()
            .border_color(tokens.border_subtle);
        for (index, editor) in self.editors.iter().enumerate() {
            let dirty = editor.dirty;
            let path = editor.path.clone();
            let close_path = path.clone();
            let active = self.selected.as_ref() == Some(&path) && self.diff.is_none();
            tabs = tabs.child(
                div()
                    .id(("editor-tab", index))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(6.0))
                    .h_full()
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .when(active, |s| s.bg(tokens.card_hover))
                    .child(
                        svg()
                            .path(crate::file_icons::icon_for_path(&path.to_string_lossy()))
                            .size(px(15.0))
                            .text_color(tokens.text_secondary),
                    )
                    .child(format!(
                        "{}{}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        if dirty { " •" } else { "" }
                    ))
                    .child(div().id(("close-editor", index)).child("×").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if let Some(editor) = this.editors.iter().find(|e| e.path == close_path)
                                && (editor.saving || editor.dirty)
                            {
                                this.message = "Save your changes before closing this tab.".into();
                                cx.notify();
                                return;
                            }
                            this.editors.retain(|e| e.path != close_path);
                            if this.selected.as_ref() == Some(&close_path) {
                                this.selected = this.editors.last().map(|e| e.path.clone());
                            }
                            this.focus_selected(window, cx);
                            this.changed(cx);
                        }),
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.select_editor(path.clone(), window, cx);
                        }),
                    ),
            );
        }
        let input = self
            .diff
            .as_ref()
            .map(|diff| diff.input.clone())
            .or_else(|| {
                self.editors
                    .iter()
                    .find(|e| Some(&e.path) == self.selected.as_ref())
                    .map(|e| e.input.clone())
            });
        let body = if let Some(input) = input {
            div()
                .flex_1()
                .min_h_0()
                .child(
                    Input::new(&input)
                        .appearance(false)
                        .bordered(false)
                        .font_family(crate::MONO_FONT)
                        .text_size(px(13.0))
                        .line_height(px(21.0))
                        .size_full(),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(tokens.text_muted)
                .child("Select a file to edit, or a Git change to review")
                .into_any_element()
        };
        let mut tools = div().flex().items_center().gap(px(10.0));
        if self.diff.is_some() {
            tools = tools.child(
                div()
                    .id("edit-diff-file")
                    .cursor_pointer()
                    .child("Edit working file")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            let path = this.diff.as_ref().and_then(|diff| diff.edit_path.clone());
                            if let Some(path) = path {
                                this.open(
                                    SavedFile {
                                        path,
                                        ..Default::default()
                                    },
                                    true,
                                    window,
                                    cx,
                                );
                            } else {
                                this.message = "This file is outside the project folder.".into();
                                cx.notify();
                            }
                        }),
                    ),
            );
        } else if self.selected.is_some() {
            tools = tools
                .child(
                    div()
                        .id("reload-file")
                        .cursor_pointer()
                        .child("Discard draft & reload")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| this.reload_selected(window, cx)),
                        ),
                )
                .child(
                    div()
                        .id("save-file")
                        .cursor_pointer()
                        .child("Save  Ctrl/Cmd+S")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.save_selected(cx)),
                        ),
                );
        }
        div()
            .debug_selector(|| "files-workspace".into())
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().editor_background())
            .text_color(tokens.text_primary)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
                    && event.keystroke.key == "s"
                {
                    cx.stop_propagation();
                    this.save_selected(cx);
                }
            }))
            .child(
                div().flex_1().min_h_0().flex().child(sidebar).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(tabs)
                        .child(
                            div()
                                .h(px(28.0))
                                .px(px(10.0))
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_size(px(11.0))
                                .text_color(tokens.text_secondary)
                                .child(
                                    self.diff
                                        .as_ref()
                                        .map(|diff| &diff.repository_path)
                                        .or(self.selected.as_ref())
                                        .map_or(String::new(), |p| {
                                            p.to_string_lossy().into_owned()
                                        }),
                                )
                                .child(tools),
                        )
                        .child(body),
                ),
            )
            .child(
                div()
                    .h(px(24.0))
                    .px(px(10.0))
                    .text_size(px(11.0))
                    .text_color(tokens.text_secondary)
                    .truncate()
                    .child(self.message.clone()),
            )
            .into_any_element()
    }
}
