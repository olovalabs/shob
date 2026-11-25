impl AppState {
    fn render_agent_permission(
        &self,
        pane: &TerminalPane,
        in_picker: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use agent_bridge::PermissionMode;

        let pane_id = pane.id;
        let tokens = self.tokens(cx);
        let current = pane.chat.permission_mode;
        let options = [
            (
                PermissionMode::Ask,
                "icons/permission-hand.svg",
                "Ask before changes, commands, and other actions",
            ),
            (
                PermissionMode::Auto,
                "icons/permission-auto.svg",
                "Allow edits to existing workspace files; ask for other actions",
            ),
            (
                PermissionMode::Full,
                "icons/permission-full.svg",
                "Run tools without approval prompts",
            ),
        ];
        let icon = options[current as usize].1;
        let toggle_bounds = pane.permission_toggle_bounds.clone();
        let outside_bounds = toggle_bounds.clone();
        let menu_position = toggle_bounds.get().origin;
        let id = if in_picker {
            "picker-permissions"
        } else {
            "agent-permissions"
        };
        let accent = tokens.block_warning;
        let composer_bg = tokens.card_hover;
        let menu_bg = if in_picker { tokens.model_picker_bg } else { tokens.card_bg };
        let menu_border = tokens.card_border;
        let pill_color = if current == PermissionMode::Full {
            accent
        } else {
            tokens.text_primary
        };

        div()
            .relative()
            .child(
                div()
                    .id((id, pane_id))
                    .debug_selector(move || id.into())
                    .relative()
                    .flex_shrink_0()
                    .h(px(28.0))
                    .px(px(7.0))
                    .rounded(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .hover(move |button| {
                        button.bg(if in_picker {
                            tokens.card_hover
                        } else {
                            tokens.pane_header_btn_hover
                        })
                    })
                    .when(!in_picker, |button| {
                        button
                            .rounded_full()
                            .px(px(9.0))
                            .h(px(26.0))
                            .text_size(px(12.0))
                            .text_color(pill_color)
                            .gap(px(5.0))
                            .bg(composer_bg)
                            .child(
                                canvas(
                                    move |bounds, _, _| toggle_bounds.set(bounds),
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full(),
                            )
                    })
                    .child(
                        svg()
                            .path(icon)
                            .size(if in_picker { px(13.0) } else { px(14.0) })
                            .flex_shrink_0()
                            .when(!in_picker, |icon| icon.text_color(pill_color)),
                    )
                    .child(current.label())
                    .when(in_picker, |button| {
                        button.child(
                            svg()
                                .path("icons/chevron-down.svg")
                                .size(px(10.0))
                                .with_transformation(gpui::Transformation::rotate(gpui::radians(
                                    std::f32::consts::PI,
                                ))),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(pane) = this.pane_mut(pane_id) {
                                pane.permission_menu_open = !pane.permission_menu_open;
                                cx.notify();
                            }
                        }),
                    ),
            )
            .when(
                pane.permission_menu_open && (in_picker || pane.chat.auth.is_none()),
                |button| {
                    let menu = div()
                        .id(("permission-popover", pane_id))
                        .debug_selector(|| "agent-permission-menu".into())
                        .w(if in_picker { px(200.0) } else { px(420.0) })
                        .max_w((window.viewport_size().width - px(24.0)).max(px(0.0)))
                        .p(px(5.0))
                        .rounded(if in_picker { px(8.0) } else { px(12.0) })
                        .border_1()
                        .border_color(menu_border)
                        .bg(menu_bg)
                        .shadow_lg()
                        .occlude()
                        .text_size(px(12.0))
                        .text_color(tokens.text_primary)
                        .when(!in_picker, |menu| {
                            menu.child(
                                div()
                                    .px(px(8.0))
                                    .pt(px(5.0))
                                    .pb(px(6.0))
                                    .text_color(tokens.text_muted)
                                    .child("Approval mode applies to all agent chats"),
                            )
                            .on_mouse_down_out(cx.listener(
                                move |this, event: &MouseDownEvent, _, cx| {
                                    if !outside_bounds.get().contains(&event.position)
                                        && let Some(pane) = this.pane_mut(pane_id)
                                    {
                                        pane.permission_menu_open = false;
                                        cx.notify();
                                    }
                                },
                            ))
                        })
                        .when(agent_bridge::rpc_backend_requested(), |menu| {
                            menu.child(
                                div()
                                    .p(px(6.0))
                                    .text_color(tokens.text_muted)
                                    .child("The legacy RPC backend does not enforce this mode."),
                            )
                        })
                        .children(options.map(|(mode, icon, description)| {
                            let active = current == mode;
                            let foreground = if active && !in_picker {
                                accent
                            } else {
                                tokens.text_primary
                            };
                            let detail = if active { accent } else { tokens.text_muted };
                            div()
                                .id(("permission-option", mode as usize))
                                .debug_selector(move || {
                                    format!("agent-permission-option-{}", mode as usize)
                                })
                                .px(px(8.0))
                                .py(px(5.0))
                                .min_h(px(30.0))
                                .rounded(px(4.0))
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .cursor_pointer()
                                .hover(move |row| {
                                    row.bg(if in_picker {
                                        tokens.card_hover
                                    } else {
                                        tokens.pane_header_btn_hover
                                    })
                                })
                                .when(!in_picker, |row| {
                                    row.items_start().gap(px(9.0)).rounded(px(6.0)).child(
                                        svg()
                                            .path(icon)
                                            .size(px(15.0))
                                            .mt(px(3.0))
                                            .flex_shrink_0()
                                            .text_color(foreground),
                                    )
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_size(if in_picker {
                                                    px(12.0)
                                                } else {
                                                    px(13.0)
                                                })
                                                .text_color(foreground)
                                                .child(mode.label()),
                                        )
                                        .when(!in_picker, |label| {
                                            label.child(
                                                div()
                                                    .text_size(px(12.0))
                                                    .text_color(detail)
                                                    .child(description),
                                            )
                                        }),
                                )
                                .when(active, |row| {
                                    row.child(
                                        svg()
                                            .path("icons/status-check.svg")
                                            .size(px(12.0))
                                            .flex_shrink_0()
                                            .when(!in_picker, |check| {
                                                check.size(px(14.0)).mt(px(5.0)).text_color(accent)
                                            }),
                                    )
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.set_permission_mode(mode, cx);
                                    }),
                                )
                        }));

                    if in_picker {
                        button.child(menu.absolute().right_0().bottom(px(34.0)))
                    } else {
                        button.child(deferred(
                            gpui::anchored()
                                .anchor(gpui::Anchor::BottomLeft)
                                .position(menu_position)
                                .offset(gpui::point(px(0.0), px(-4.0)))
                                .child(menu),
                        ))
                    }
                },
            )
    }
}
