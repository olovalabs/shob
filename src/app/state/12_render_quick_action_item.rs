#[derive(PartialEq, Eq)]
struct PanePathProbe {
    project: PathBuf,
    cwd: Option<PathBuf>,
    workspace_override: Option<PathBuf>,
    pid: Option<u32>,
    chat: bool,
}

#[derive(PartialEq, Eq)]
struct PanePathSample {
    source: PanePathProbe,
    shell_dir: PathBuf,
    branch: Option<SharedString>,
    working_dir_exists: bool,
    override_is_distinct: bool,
}

impl PanePathProbe {
    fn matches(&self, pane: &TerminalPane, project: &Path) -> bool {
        self.project == project
            && self.cwd == pane.cwd
            && self.workspace_override == pane.chat.workspace_override
            && self.pid == pane.pid
            && self.chat == pane.chat.active
    }

    fn sample(self) -> PanePathSample {
        #[cfg(not(target_os = "linux"))]
        let shell_dir = None;
        #[cfg(target_os = "linux")]
        let shell_dir = self.pid
            .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok());
        let shell_dir = shell_dir
            .or_else(|| self.cwd.as_ref().filter(|path| path.is_dir()).cloned())
            .unwrap_or_else(|| self.project.clone());
        let working_dir = if self.chat {
            self.workspace_override.as_deref().unwrap_or(&shell_dir)
        } else {
            &shell_dir
        };
        let branch = git_workspace::detect_git_branch(working_dir).map(SharedString::from);
        let working_dir_exists = working_dir.is_dir();
        let override_is_distinct = self.workspace_override.as_deref()
            .is_some_and(|path| !git_workspace::paths_equal(path, &self.project));
        PanePathSample { source: self, shell_dir, branch, working_dir_exists, override_is_distinct }
    }
}

impl AppState {
    fn render_quick_action_item(
        label: &'static str,
        cmd: &'static str,
        icon: &'static str,
        pane_id: usize,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(format!("quick-action-{pane_id}-{cmd}")))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(4.0))
            .cursor(CursorStyle::PointingHand)
            .hover(|s| s.bg(tokens.pane_header_btn_hover))
            .child(
                svg()
                    .path(icon)
                    .size(px(13.0))
                    .text_color(tokens.text_muted),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(tokens.text_primary)
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .font_family(MONO_FONT)
                    .text_color(tokens.text_muted)
                    .ml_auto()
                    .child(cmd),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.run_quick_action(pane_id, cmd, window, cx);
                }),
            )
    }

    fn agent_working_directory(&self, pane: &TerminalPane) -> PathBuf {
        self.agent_working_directory_ref(pane).to_path_buf()
    }

    fn agent_working_directory_ref<'a>(&'a self, pane: &'a TerminalPane) -> &'a Path {
        pane.chat.workspace_override.as_deref()
            .or_else(|| self.pane_cwd_ref(pane))
            .unwrap_or_else(|| Path::new("."))
    }

    fn pane_project_path(&self, pane: &TerminalPane) -> Option<&Path> {
        self.pane_session_dir(pane.id).map(PathBuf::as_path)
    }

    fn pane_session_dir(&self, pane_id: usize) -> Option<&PathBuf> {
        let &(project, session, _) = self.pane_index.get(&pane_id)?;
        let project = self.projects.get(project)?;
        Some(project.session_dir(project.sessions.get(session)?))
    }

    fn pane_path_sample<'a>(&self, pane: &'a TerminalPane) -> Option<&'a PanePathSample> {
        let project = self.pane_project_path(pane)?;
        pane.path_sample.as_ref().filter(|sample| sample.source.matches(pane, project))
    }

    fn pane_cwd_ref<'a>(&'a self, pane: &'a TerminalPane) -> Option<&'a Path> {
        self.pane_path_sample(pane).map(|sample| sample.shell_dir.as_path())
            .or(pane.cwd.as_deref())
            .or_else(|| self.pane_project_path(pane))
    }

    fn pane_cwd_path(&self, pane: &TerminalPane) -> Option<PathBuf> {
        self.pane_cwd_ref(pane).map(Path::to_path_buf)
    }

    fn pane_git_branch(&self, pane: &TerminalPane) -> Option<SharedString> {
        self.pane_path_sample(pane)?.branch.clone()
    }

    fn active_path_requests(&self) -> Vec<(usize, PanePathProbe)> {
        if self.active_view != ActiveView::Workspace {
            return Vec::new();
        }
        let Some(project) = self.active_project() else { return Vec::new() };
        let Some(session) = self.active_session() else { return Vec::new() };
        session.panes.iter()
            .filter(|pane| !pane.is_browser())
            .map(|pane| (pane.id, PanePathProbe {
                project: project.session_dir(session).clone(),
                cwd: pane.cwd.clone(),
                workspace_override: pane.chat.workspace_override.clone(),
                pid: pane.pid,
                chat: pane.chat.active,
            }))
            .collect()
    }

    fn apply_path_samples(&mut self, samples: Vec<(usize, PanePathSample)>, cx: &mut Context<Self>) {
        let mut changed = false;
        for (id, sample) in samples {
            let Some(pane) = self.pane_ref(id) else { continue };
            if !self.pane_project_path(pane).is_some_and(|project| sample.source.matches(pane, project))
                || pane.path_sample.as_ref() == Some(&sample)
            {
                continue;
            }
            if sample.source.chat && sample.source.workspace_override.is_some()
                && !sample.working_dir_exists && !pane.chat.running
            {
                self.apply_workspace_override(id, None, cx);
            } else if let Some(pane) = self.pane_mut(id) {
                pane.path_sample = Some(sample);
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn format_path_display(path: &std::path::Path) -> String {
        static HOME_DIR: std::sync::LazyLock<Option<PathBuf>> = std::sync::LazyLock::new(|| {
            let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
            std::env::var_os(name).map(PathBuf::from)
        });
        let home = HOME_DIR.as_ref();
        if let Some(rel) = home.and_then(|h| path.strip_prefix(h).ok()) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            } else {
                return format!("~/{}", rel.display());
            }
        }
        path.display().to_string()
    }

    /// Thumbnails of images staged for the next prompt, shown between the
    /// badges row and the text editor.
    fn render_attachment_strip(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if pane.attachments.is_empty() {
            return None;
        }

        let pane_id = pane.id;
        let border: Hsla = if tokens.is_dark {
            rgba(0x34353cff).into()
        } else {
            rgba(0x0000001f).into()
        };
        let hover_border: Hsla = tokens.ring;

        let thumbnails = pane
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| {
                let image_path = attachment.path.clone();
                let is_image = attachment.is_image;
                let name = attachment.name.clone();
                let icon = file_icons::icon_for_path(&image_path.to_string_lossy());
                let attachment = attachment.clone();

                div()
                    .id(SharedString::from(format!(
                        "attachment-thumb-{pane_id}-{index}"
                    )))
                    .relative()
                    .size(px(52.0))
                    .rounded(px(7.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(border)
                    .bg(rgba(0x00000022))
                    .cursor(CursorStyle::PointingHand)
                    .hover(move |style| style.border_color(hover_border))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if attachment.is_image {
                                this.open_image_viewer(&attachment, cx);
                            }
                        }),
                    )
                    .when(is_image, |this| {
                        this.child(img(image_path).size_full().object_fit(ObjectFit::Cover))
                    })
                    .when(!is_image, |this| {
                        this.flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(2.0))
                            .child(svg().path(icon).size(px(22.0)))
                            .child(
                                div()
                                    .w_full()
                                    .px(px(2.0))
                                    .text_size(px(9.0))
                                    .overflow_hidden()
                                    .child(name),
                            )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "attachment-remove-{pane_id}-{index}"
                            )))
                            .absolute()
                            .top(px(2.0))
                            .right(px(2.0))
                            .size(px(15.0))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgba(0x000000b8))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|style| style.bg(rgba(0x000000e0)))
                            .child(
                                svg()
                                    .path("icons/x.svg")
                                    .size(px(9.0))
                                    .text_color(gpui::white()),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.remove_attachment(pane_id, index, cx);
                                }),
                            ),
                    )
                    .into_any_element()
            });

        Some(
            div()
                .id(("attachment-strip", pane_id))
                .w_full()
                .mt(px(10.0))
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .children(thumbnails)
                .into_any_element(),
        )
    }

    /// Full-window lightbox opened by clicking a thumbnail.
    fn render_image_viewer_layer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(viewer) = self.image_viewer.as_ref() else {
            return div().into_any_element();
        };
        let tokens = self.tokens(cx);
        let path = viewer.path.clone();
        let name = viewer.name.clone();
        let subtitle = viewer
            .dimensions
            .map(|(width, height)| format!("{width} × {height}"))
            .unwrap_or_default();

        let panel_bg: Hsla = if tokens.is_dark {
            rgba(0x101114ff).into()
        } else {
            rgba(0xffffffff).into()
        };
        let panel_border: Hsla = if tokens.is_dark {
            rgba(0x2d2e3dff).into()
        } else {
            tokens.card_border
        };
        let image_bg: Hsla = if tokens.is_dark {
            rgba(0x00000066).into()
        } else {
            rgba(0x00000010).into()
        };

        deferred(
            div()
                .id("image-viewer-layer")
                .size_full()
                .absolute()
                .top_0()
                .left_0()
                .child(
                    div()
                        .id("image-viewer-backdrop")
                        .size_full()
                        .absolute()
                        .top_0()
                        .left_0()
                        .bg(rgba(0x000000b8))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.image_viewer = None;
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(52.0))
                        .child(
                            div()
                                .id("image-viewer-panel")
                                .relative()
                                .occlude()
                                .w_full()
                                .h_full()
                                .max_w(px(1100.0))
                                .max_h(px(860.0))
                                .rounded(px(12.0))
                                .border_1()
                                .border_color(panel_border)
                                .bg(panel_bg)
                                .shadow_lg()
                                .overflow_hidden()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap(px(12.0))
                                        .flex_shrink_0()
                                        .h(px(46.0))
                                        .px(px(14.0))
                                        .border_b_1()
                                        .border_color(panel_border)
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap(px(1.0))
                                                .min_w(px(0.0))
                                                .child(
                                                    div()
                                                        .text_size(px(12.5))
                                                        .font_weight(gpui::FontWeight::BOLD)
                                                        .text_color(tokens.text_primary)
                                                        .child(name),
                                                )
                                                .when(!subtitle.is_empty(), |this| {
                                                    this.child(
                                                        div()
                                                            .text_size(px(10.0))
                                                            .text_color(tokens.text_muted)
                                                            .child(subtitle),
                                                    )
                                                }),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(10.0))
                                                .child(
                                                    div()
                                                        .text_size(px(10.0))
                                                        .text_color(tokens.text_muted)
                                                        .child("Esc or click outside to close"),
                                                )
                                                .child(
                                                    div()
                                                        .id("image-viewer-close")
                                                        .size(px(26.0))
                                                        .rounded(px(6.0))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .cursor(CursorStyle::PointingHand)
                                                        .hover(|style| {
                                                            style.bg(tokens.pane_header_btn_hover)
                                                        })
                                                        .child(
                                                            svg()
                                                                .path("icons/x.svg")
                                                                .size(px(12.0))
                                                                .text_color(tokens.text_muted),
                                                        )
                                                        .on_mouse_down(
                                                            MouseButton::Left,
                                                            cx.listener(|this, _, _, cx| {
                                                                cx.stop_propagation();
                                                                this.image_viewer = None;
                                                                cx.notify();
                                                            }),
                                                        ),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_h(px(0.0))
                                        .bg(image_bg)
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            img(path).size_full().object_fit(ObjectFit::Contain),
                                        ),
                                ),
                        ),
                ),
        )
        .into_any_element()
    }

    fn apply_task_list(
        pane: &mut TerminalPane,
        list: embedded::task_list::TaskList,
        cx: &mut Context<Self>,
    ) -> bool {
        let checklist = &mut pane.chat.checklist;
        if checklist.list.as_ref() == Some(&list) {
            return false;
        }
        let was_completed = checklist.list.as_ref().is_some_and(task_list_completed);
        let completed = task_list_completed(&list);
        let active = active_task_index(&list);
        if !checklist.browsing
            && let Some(index) = active
            && checklist.list.as_ref().and_then(active_task_index) != active
        {
            checklist.scroll.scroll_to_item(index);
        }
        checklist.list = Some(list);

        if !completed {
            checklist.dismissal = None;
        } else if !was_completed {
            let pane_id = pane.id;
            let timer = cx.background_executor().timer(TASK_LIST_COMPLETION_DELAY);
            checklist.dismissal = Some(cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(pane) = this.pane_mut(pane_id) {
                        pane.chat.checklist.dismissal = None;
                        cx.notify();
                    }
                });
            }));
        }
        true
    }

    fn render_agent_task_list(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        use embedded::task_list::TaskStatus;
        use gpui_component::{Icon, IconName, spinner::Spinner};

        if !pane.chat.active || pane.chat.worker_view.is_some() {
            return None;
        }
        let checklist = &pane.chat.checklist;
        let list = checklist.list.as_ref()?;
        let pane_id = pane.id;
        if task_list_completed(list) && checklist.dismissal.is_none() {
            return None;
        }
        Some(
            div()
                .id(("agent-task-list", pane.id))
                .debug_selector(|| "agent-task-list".to_string())
                .w_full()
                .flex_none()
                .max_h(px(152.0))
                .overflow_y_scroll()
                .track_scroll(&checklist.scroll)
                .hover_listener_mode(gpui::HoverListenerMode::InputModalityIndependent)
                .on_hover(cx.listener(move |this, hovered, _, cx| {
                    let Some(pane) = this.pane_mut(pane_id) else {
                        return;
                    };
                    let checklist = &mut pane.chat.checklist;
                    let was_browsing = std::mem::replace(&mut checklist.browsing, *hovered);
                    if was_browsing
                        && !hovered
                        && pane.chat.running
                        && let Some(index) = checklist.list.as_ref().and_then(active_task_index)
                    {
                        checklist.scroll.scroll_to_item(index);
                        cx.notify();
                    }
                }))
                .flex()
                .flex_col()
                .gap(px(5.0))
                .px(px(11.0))
                .py(px(8.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(tokens.border_subtle)
                .bg(tokens.pane_bg)
                .text_size(px(13.0))
                .children(list.tasks.iter().enumerate().map(|(index, task)| {
                    let status = match task.status {
                        TaskStatus::InProgress if !pane.chat.running => TaskStatus::Pending,
                        status => status,
                    };
                    let marker = match status {
                        TaskStatus::Completed => div()
                            .size(px(14.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(tokens.text_secondary)
                            .child(
                                Icon::new(IconName::Check)
                                    .with_size(px(11.0))
                                    .text_color(tokens.pane_bg),
                            )
                            .into_any_element(),
                        TaskStatus::InProgress => Spinner::new()
                            .icon(Icon::new(IconName::LoaderCircle))
                            .with_size(px(14.0))
                            .color(tokens.text_secondary)
                            .into_any_element(),
                        TaskStatus::Pending => div()
                            .size(px(13.0))
                            .rounded_full()
                            .border_1()
                            .border_dashed()
                            .border_color(tokens.text_muted)
                            .into_any_element(),
                    };
                    div()
                        .id(("agent-task", index))
                        .flex()
                        .flex_none()
                        .items_start()
                        .gap(px(6.0))
                        .child(
                            div()
                                .id("task-status")
                                .debug_selector(move || match status {
                                    TaskStatus::Pending => "agent-task-pending".to_string(),
                                    TaskStatus::InProgress => "agent-task-running".to_string(),
                                    TaskStatus::Completed => "agent-task-completed".to_string(),
                                })
                                .flex_none()
                                .size(px(16.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(marker),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(if task.status == TaskStatus::Completed {
                                    tokens.text_secondary
                                } else {
                                    tokens.text_primary
                                })
                                .child(task.text.clone()),
                        )
                })),
        )
    }
}
