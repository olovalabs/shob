use super::*;
use gpui_component::button::ButtonVariants as _;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};

impl AppState {
    pub(crate) fn conversation_header_directory(&self) -> Option<&Path> {
        let pane = self.focused_conversation_pane()?;
        pane.chat
            .workspace_override
            .as_deref()
            .or(pane.cwd.as_deref())
            .or_else(|| self.active_project().map(|project| project.path.as_path()))
    }

    pub(crate) fn focused_conversation_pane(&self) -> Option<&TerminalPane> {
        if !self.settings.agent_layout {
            return None;
        }
        let pane = self.one_mode_pane(self.active_session()?)?;
        (pane.chat.active && pane.diff_preview.is_none() && pane.markdown_preview.is_none())
            .then_some(pane)
    }

    fn conversation_header_title(&self, session: &Session) -> SharedString {
        if self.sidebar_session_has_title(session) && !session.title.trim().is_empty() {
            session.title.clone()
        } else {
            "New code session".into()
        }
    }

    pub(crate) fn render_conversation_header(
        &self,
        pane_id: usize,
        wide: bool,
        info_visible: bool,
        tokens: ThemeTokens,
        cx: &Context<Self>,
    ) -> gpui::Stateful<Div> {
        div()
            .id("agent-conversation-header")
            .debug_selector(|| "agent-conversation-header".into())
            .w_full()
            .h(px(38.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .pl(px(22.0))
            .pr(px(16.0))
            .border_b_1()
            .border_color(tokens.border)
            .text_size(px(12.5))
            .text_color(tokens.text_muted)
            .child(self.render_conversation_breadcrumb(tokens, cx))
            .child(div().flex_1())
            .child(self.conversation_info_button(pane_id, wide, info_visible, tokens, cx))
    }

    fn conversation_info_button(
        &self,
        pane_id: usize,
        wide: bool,
        info_visible: bool,
        tokens: ThemeTokens,
        cx: &Context<Self>,
    ) -> gpui::Stateful<Div> {
        div()
            .id(("focused-agent-info-toggle", pane_id))
            .debug_selector(|| "focused-agent-info-toggle".into())
            .occlude()
            .size(px(32.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|style| style.bg(tokens.pane_header_btn_hover))
            .child(
                svg()
                    .path("icons/info.svg")
                    .size(px(16.0))
                    .text_color(tokens.text_secondary),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.editor_picker_open = None;
                    if let Some(pane) = this.pane_mut(pane_id) {
                        if wide {
                            pane.chat.info_open = Some(!info_visible);
                        } else {
                            pane.chat.info_open_compact = !info_visible;
                        }
                        cx.notify();
                    }
                }),
            )
    }

    fn render_conversation_breadcrumb(&self, tokens: ThemeTokens, cx: &Context<Self>) -> Div {
        let row = div().flex().min_w_0().items_center().gap(px(8.0));
        let Some(project) = self.active_project() else {
            return row.child("Shob");
        };
        let active_session = self.active_session();
        let state = cx.weak_entity();
        let branch = self
            .conversation_header_branch
            .as_ref()
            .and_then(|(dir, branch)| {
                (self.conversation_header_directory() == Some(dir.as_path()))
                    .then_some(branch.clone())
            });
        row.child(sidebar_project_icon(project, tokens))
            .child(
                div()
                    .min_w_0()
                    .max_w(px(140.0))
                    .truncate()
                    .child(project.name.clone()),
            )
            .when_some(active_session, |row, session| {
                let title = self.conversation_header_title(session);
                row.child("/").child(
                    div()
                        .id("conversation-session-switcher")
                        .debug_selector(|| "conversation-session-switcher".into())
                        .occlude()
                        .flex_none()
                        .track_focus(&self.conversation_header_menu_focus)
                        .child(
                            Button::new("conversation-switch-session")
                                .ghost()
                                .compact()
                                .h(px(24.0))
                                .px(px(3.0))
                                .cursor_pointer()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(5.0))
                                        .text_size(px(12.5))
                                        .text_color(tokens.text_primary)
                                        .child(div().max_w(px(172.0)).truncate().child(title))
                                        .child(
                                            svg()
                                                .path("icons/chevron-down.svg")
                                                .size(px(12.0))
                                                .text_color(tokens.text_primary),
                                        ),
                                )
                                .dropdown_menu(move |menu, _, cx| {
                                    let menu = menu.max_w(px(320.0)).scrollable(true);
                                    let Some(app) = state.upgrade() else {
                                        return menu;
                                    };
                                    let app = app.read(cx);
                                    let Some(project) = app.active_project() else {
                                        return menu;
                                    };
                                    project.sessions.iter().rev().fold(menu, |menu, session| {
                                        let state = state.clone();
                                        let id = session.id;
                                        let title = app.conversation_header_title(session);
                                        menu.item(
                                            PopupMenuItem::element(move |_, _| {
                                                div()
                                                    .id(("conversation-session", id))
                                                    .debug_selector(move || {
                                                        format!("conversation-session-{id}")
                                                    })
                                                    .min_w_0()
                                                    .truncate()
                                                    .child(title.clone())
                                            })
                                            .checked(
                                                app.active_session().is_some_and(|s| s.id == id),
                                            )
                                            .on_click(move |_, window, cx| {
                                                let _ = state.update(cx, |app, cx| {
                                                    if let Some(&(project, session)) =
                                                        app.session_index.get(&id)
                                                    {
                                                        app.select_session(
                                                            project, session, window, cx,
                                                        );
                                                    }
                                                });
                                            }),
                                        )
                                    })
                                }),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .gap(px(5.0))
                    .ml(px(6.0))
                    .child(
                        svg()
                            .path("icons/monitor.svg")
                            .size(px(13.0))
                            .text_color(tokens.text_muted),
                    )
                    .child("Local"),
            )
            .when_some(branch, |row, branch| {
                row.child("·").child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .min_w_0()
                        .child(
                            svg()
                                .path("icons/git-branch.svg")
                                .size(px(13.0))
                                .flex_none()
                                .text_color(tokens.text_muted),
                        )
                        .child(div().max_w(px(140.0)).truncate().child(branch)),
                )
            })
    }
}
