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
        pane.chat
            .workspace_override
            .clone()
            .or_else(|| self.pane_cwd_path(pane))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn pane_cwd_path(&self, pane: &TerminalPane) -> Option<std::path::PathBuf> {
        #[cfg(target_os = "linux")]
        if let Some(pid) = pane.pid {
            // The input bar asks for this every frame. `/proc/<pid>/cwd` and the
            // git lookup below only change when the shell/user moves, so a short
            // TTL removes the syscalls from the render path.
            type ProcCwdCache = HashMap<u32, (std::time::Instant, Option<PathBuf>)>;
            static PROC_CWD_CACHE: std::sync::LazyLock<Mutex<ProcCwdCache>> =
                std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
            let now = std::time::Instant::now();
            let cached = PROC_CWD_CACHE.lock().get(&pid).and_then(|(at, path)| {
                (now.duration_since(*at) < std::time::Duration::from_secs(1)).then(|| path.clone())
            });
            if let Some(path) = cached {
                if path.is_some() {
                    return path;
                }
            } else {
                let link = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
                PROC_CWD_CACHE.lock().insert(pid, (now, link.clone()));
                if link.is_some() {
                    return link;
                }
            }
        }
        if let Some(cwd) = pane.cwd.clone().filter(|path| path.is_dir()) {
            return Some(cwd);
        }
        if let Some(project) = self
            .pane_index
            .get(&pane.id)
            .and_then(|&(p_idx, _, _)| self.projects.get(p_idx))
        {
            return Some(project.path.clone());
        }
        directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
    }

    fn pane_git_branch(&self, pane: &TerminalPane) -> Option<String> {
        let dir = if pane.chat.active {
            self.agent_working_directory(pane)
        } else {
            self.pane_cwd_path(pane)?
        };
        Self::detect_git_branch(&dir)
    }

    fn detect_git_branch(dir: &std::path::Path) -> Option<String> {
        git_workspace::detect_git_branch(dir)
    }

    fn pane_cwd_display(&self, pane: &TerminalPane) -> String {
        if let Some(path) = self.pane_cwd_path(pane) {
            Self::format_path_display(&path)
        } else {
            "~".to_string()
        }
    }

    fn format_path_display(path: &std::path::Path) -> String {
        // Resolving the home directory walks platform config; it is constant
        // for the process.
        static HOME_DIR: std::sync::LazyLock<Option<PathBuf>> = std::sync::LazyLock::new(|| {
            directories::UserDirs::new()
                .map(|u| u.home_dir().to_path_buf())
                .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
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
