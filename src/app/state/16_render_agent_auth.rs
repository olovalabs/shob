impl AppState {
    fn render_agent_auth(
        &self,
        pane_id: usize,
        auth: &AuthModal,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tokens = self.tokens(cx);
        let themed_input = self.palette_scheme.is_imported()
            || self.palette_scheme == PaletteScheme::MidnightGarden && tokens.is_dark;
        let filter_focused = auth.filter.read(cx).focus_handle().is_focused(window);
        let provider_focused = auth
            .provider_input
            .read(cx)
            .focus_handle()
            .is_focused(window);
        let credential_focused = auth.input.read(cx).focus_handle().is_focused(window);
        let filter_query = auth.filter.read(cx).value().trim().to_lowercase();

        let mut providers: Vec<&agent_auth::Provider> = auth
            .providers
            .iter()
            .filter(|provider| {
                agent_auth::provider_visible_in_chat_ui(&provider.id)
                    && provider.source.as_deref() != Some("anonymous")
            })
            .filter(|provider| {
                if filter_query.is_empty() {
                    return true;
                }
                let (name, desc) = provider_metadata(&provider.id);
                provider
                    .name
                    .as_ref()
                    .is_some_and(|name| name.to_lowercase().contains(&filter_query))
                    || provider.id.to_lowercase().contains(&filter_query)
                    || (!name.is_empty() && name.to_lowercase().contains(&filter_query))
                    || (!desc.is_empty() && desc.to_lowercase().contains(&filter_query))
            })
            .collect();
        providers.sort_by_key(|provider| {
            (
                !provider.ready,
                provider_featured_rank(&provider.id),
                provider.id.clone(),
            )
        });
        let modal_divider = tokens.border_subtle;

        let background = tokens.card_bg;
        let inset = tokens.card_hover;
        let card = tokens.pane_bg;
        let border = tokens.card_border;
        let muted = tokens.text_muted;
        let foreground = tokens.text_primary;

        let mut badge_success_bg = tokens.block_success;
        badge_success_bg.a = if tokens.is_dark { 0.18 } else { 0.14 };
        let mut badge_success_border = tokens.block_success;
        badge_success_border.a = if tokens.is_dark { 0.38 } else { 0.28 };

        let mut badge_failure_bg = tokens.block_failure;
        badge_failure_bg.a = if tokens.is_dark { 0.18 } else { 0.14 };
        let mut badge_failure_border = tokens.block_failure;
        badge_failure_border.a = if tokens.is_dark { 0.38 } else { 0.28 };

        let btn_contrast_bg = tokens.pane_bg;
        let btn_contrast_hover = tokens.pane_header_btn_hover;

        let card_sub_bg = tokens.card_hover;
        let disconnect_bg = tokens.card_hover;

        struct ProviderCardItem {
            index: usize,
            id: String,
            name: String,
            description: String,
            ready: bool,
            selected: bool,
        }

        let card_items: Vec<ProviderCardItem> = providers
            .iter()
            .enumerate()
            .map(|(index, provider)| {
                let id = provider.id.clone();
                let (meta_name, meta_desc) = provider_metadata(&id);
                let name = if let Some(name) = &provider.name {
                    name.clone()
                } else if !meta_name.is_empty() {
                    meta_name.to_string()
                } else {
                    id.clone()
                };
                let description = meta_desc.to_string();
                let ready = provider.ready;
                let selected = auth.selected.as_deref() == Some(id.as_str());
                ProviderCardItem {
                    index,
                    id,
                    name,
                    description,
                    ready,
                    selected,
                }
            })
            .collect();

        let has_matching_providers = !card_items.is_empty();
        let provider_sizes: Vec<_> = card_items
            .chunks(2)
            .map(|pair| {
                let height = if pair.iter().any(|item| !item.description.is_empty()) {
                    58.0
                } else {
                    40.0
                };
                size(px(0.0), px(height + 8.0))
            })
            .collect();
        let provider_height: f32 = provider_sizes
            .iter()
            .map(|size| f32::from(size.height))
            .sum();
        let provider_list =
            v_virtual_list(
                cx.entity(),
                ("auth-provider-list", pane_id),
                Rc::new(provider_sizes),
                move |_, range, _, cx| {
                    range
                        .map(|row_index| {
                            let pair = &card_items
                                [row_index * 2..(row_index * 2 + 2).min(card_items.len())];
                            let row_has_description =
                                pair.iter().any(|item| !item.description.is_empty());
                            let row_height = if row_has_description { 58.0 } else { 40.0 };
                            let is_single = pair.len() == 1;

                            let mut row_children = Vec::new();
                            for item in pair {
                                let index = item.index;
                                let id = item.id.clone();
                                let name = item.name.clone();
                                let description = item.description.clone();
                                let described = !description.is_empty();
                                let ready = item.ready;
                                let selected = item.selected;
                                let icon = provider_icon_path(&id);

                                row_children.push(
                                    div()
                                        .id(("auth-provider", index))
                                        .debug_selector(move || format!("auth-provider-{index}"))
                                        .flex_1()
                                        .min_w_0()
                                        .h(px(row_height))
                                        .px(px(12.0))
                                        .rounded(px(6.0))
                                        .border_1()
                                        .border_color(if selected { tokens.ring } else { border })
                                        .bg(if selected { inset } else { card })
                                        .flex()
                                        .items_center()
                                        .gap(px(10.0))
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(move |view| {
                                            if !selected {
                                                view.bg(background)
                                            } else {
                                                view.bg(inset)
                                            }
                                        })
                                        .child(
                                            svg()
                                                .path(icon)
                                                .size(px(16.0))
                                                .flex_shrink_0()
                                                .text_color(foreground),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .flex()
                                                .flex_col()
                                                .justify_center()
                                                .gap(px(1.0))
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(13.0))
                                                        .font_weight(gpui::FontWeight::MEDIUM)
                                                        .text_color(foreground)
                                                        .child(name),
                                                )
                                                .when(described, |view| {
                                                    view.child(
                                                        div()
                                                            .truncate()
                                                            .text_size(px(12.0))
                                                            .text_color(muted)
                                                            .child(description),
                                                    )
                                                }),
                                        )
                                        .when(ready, |view| {
                                            let disconnect_id = id.clone();
                                            view.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .flex_shrink_0()
                                    .child(
                                        div()
                                            .px(px(6.0))
                                            .py(px(1.5))
                                            .rounded(px(4.0))
                                            .bg(badge_success_bg)
                                            .border_1()
                                            .border_color(badge_success_border)
                                            .text_size(px(10.5))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(tokens.block_success)
                                            .child("Connected"),
                                    )
                                    .child(
                                        div()
                                            .id(("auth-disconnect", index))
                                            .debug_selector(move || {
                                                format!("auth-disconnect-{index}")
                                            })
                                            .px(px(6.0))
                                            .py(px(2.5))
                                            .rounded(px(4.0))
                                            .bg(badge_failure_bg)
                                            .border_1()
                                            .border_color(badge_failure_border)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor(CursorStyle::PointingHand)
                                            .hover(|view| view.opacity(0.8))
                                            .tooltip(|window, cx| {
                                                gpui_component::tooltip::Tooltip::new("Disconnect")
                                                    .build(window, cx)
                                            })
                                            .child(
                                                svg()
                                                    .path("icons/x.svg")
                                                    .size(px(9.0))
                                                    .text_color(tokens.block_failure),
                                            )
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.disconnect_agent_provider(
                                                        pane_id,
                                                        disconnect_id.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                }),
                                            ),
                                    ),
                            )
                                        })
                                        .when(!ready, |card| {
                                            card.child(
                                                div()
                                                    .debug_selector(move || {
                                                        format!("auth-connect-{index}")
                                                    })
                                                    .flex_shrink_0()
                                                    .text_size(px(12.0))
                                                    .text_color(muted)
                                                    .child("Connect"),
                                            )
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                if ready {
                                                    this.set_agent_dialog_page(
                                                        pane_id,
                                                        AgentDialogPage::Models,
                                                        window,
                                                        cx,
                                                    );
                                                } else {
                                                    this.select_provider_for_auth(
                                                        pane_id,
                                                        id.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            }),
                                        ),
                                );
                            }
                            if is_single {
                                row_children.push(
                                    div()
                                        .id(("auth-provider-spacer", pair[0].index))
                                        .flex_1()
                                        .min_w_0(),
                                );
                            }

                            div()
                                .w_full()
                                .h(px(row_height + 8.0))
                                .pb(px(8.0))
                                .flex()
                                .gap(px(8.0))
                                .children(row_children)
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&auth.provider_scroll);
        div()
            .id(("agent-auth-content", pane_id))
            .flex().flex_col().flex_1().min_h_0().overflow_hidden()
            .on_action(cx.listener(move |this, _: &gpui_component::input::Enter, window, cx| {
                cx.stop_propagation();
                this.submit_agent_auth(pane_id, window, cx);
            }))
                        .child(
                            div()
                                .id(("auth-filter", pane_id))
                                .debug_selector(|| "agent-dialog-filter".into())
                                .mx(px(20.0))
                                .mb(px(10.0))
                                .flex_shrink_0()
                                .child(
                                    Input::new(&auth.filter)
                                        .prefix(
                                            div()
                                                .pl(px(2.0))
                                                .pr(px(6.0))
                                                .flex()
                                                .items_center()
                                                .child(
                                                    svg()
                                                        .path("ui_icons/search.svg")
                                                        .size(px(13.5))
                                                        .text_color(tokens.text_muted),
                                                ),
                                        )
                                        .appearance(true)
                                        .bordered(true)
                                        .cleanable(true)
                                        .when(themed_input, |input| style_auth_input(input, tokens, filter_focused)),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(px(1.0))
                                .bg(modal_divider),
                        )
                        .child(
                            div()
                                .id(("auth-provider-body", pane_id))
                                .px(px(16.0))
                                .py(px(8.0))
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scrollbar()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .when_some(auth.custom_provider.as_ref(), |view, fields| {
                                    let labels = ["ID", "Name", "Endpoint URL", "API key", "Models"];
                                    let mut form = div().flex().flex_col().gap(px(8.0))
                                        .child(div().text_size(px(14.0)).child("OpenAI-compatible API"))
                                        .child(div().text_size(px(11.0)).text_color(tokens.text_muted)
                                            .child("Use a Chat Completions base URL, including /v1 when required. Enter exact model IDs separated by commas."));
                                    for (label, field) in labels.into_iter().zip(fields) {
                                        let focused = field.read(cx).focus_handle().is_focused(window);
                                        form = form.child(div().flex().flex_col().gap(px(3.0))
                                            .child(div().text_size(px(11.5)).text_color(tokens.text_secondary).child(label))
                                            .child(Input::new(field).appearance(true).bordered(true)
                                                .when(themed_input, |input| style_auth_input(input, tokens, focused))));
                                    }
                                    view.child(form
                                        .when_some(auth.error.as_ref(), |view, error| view.child(div().text_color(tokens.block_failure).child(error.clone())))
                                        .child(div().flex().justify_end().gap(px(12.0))
                                            .child(div().id(("cancel-compatible-provider", pane_id)).debug_selector(|| "cancel-compatible-provider".into()).py(px(8.0)).cursor(CursorStyle::PointingHand).child("Cancel")
                                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    if let Some(auth) = this.pane_mut(pane_id).and_then(|p| p.chat.auth.as_mut())
                                                        && auth.provider_operation.is_none() {
                                                        auth.custom_provider = None;
                                                        auth.error = None;
                                                    }
                                                    cx.notify();
                                                })))
                                            .child(div().id(("save-compatible-provider", pane_id)).px(px(12.0)).py(px(8.0)).rounded(px(5.0)).bg(btn_contrast_bg)
                                                .cursor(CursorStyle::PointingHand)
                                                .child(if auth.provider_operation.is_some() { "Saving…" } else { "Save & Connect" })
                                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation(); this.save_custom_provider(pane_id, cx);
                                                })))))
                                })
                                .when(auth.page == AgentDialogPage::Auth && auth.custom_provider.is_none(), |view| {
                                    view.when(auth.discovery.is_some() && auth.providers.is_empty(), |view| {
                                        view.child(
                                            div()
                                                .py(px(8.0))
                                                .text_size(px(12.0))
                                                .text_color(tokens.text_muted)
                                                .child("Checking providers…"),
                                        )
                                    })
                                    .child(
                                        div()
                                            .id(("auth-providers", pane_id))
                                            .relative()
                                            .h(px(provider_height.min(if auth.selected.is_some() { 180.0 } else { 320.0 })))
                                            .child(provider_list)
                                            .child(KitScrollbar::vertical(&auth.provider_scroll)),
                                    )
                                    .when(
                                        !filter_query.is_empty()
                                            && !has_matching_providers
                                            && !auth.providers.is_empty(),
                                        |view| {
                                            view.child(
                                                div()
                                                    .py(px(12.0))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .text_size(px(12.5))
                                                    .text_color(tokens.text_muted)
                                                    .child("No matching providers."),
                                            )
                                        },
                                    )
                                    .when(
                                        auth.discovery.is_none() && auth.providers.is_empty(),
                                        |view| {
                                            view.child(
                                                div()
                                                    .id(("auth-retry", pane_id))
                                                    .py(px(8.0))
                                                    .text_size(px(12.0))
                                                    .text_color(tokens.text_secondary)
                                                    .cursor(CursorStyle::PointingHand)
                                                    .hover(|v| v.text_color(tokens.text_primary))
                                                    .child("Retry provider discovery")
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(move |this, _, _, cx| {
                                                            cx.stop_propagation();
                                                            if let Some(auth) = this
                                                                .pane_mut(pane_id)
                                                                .and_then(|pane| pane.chat.auth.as_mut())
                                                            {
                                                                auth.error = None;
                                                                auth.discover();
                                                                cx.notify();
                                                            }
                                                        }),
                                                    ),
                                            )
                                        },
                                    )
                                    .when_some(auth.error.as_ref(), |view, error| {
                                        view.child(
                                            div()
                                                .py(px(4.0))
                                                .text_size(px(11.5))
                                                .text_color(tokens.block_failure)
                                                .child(error.clone()),
                                        )
                                    })
                                    .when_some(auth.selected.as_ref(), |view, selected| {
                                        let provider_name = auth.provider_name(selected);
                                        let methods = agent_auth::auth_methods(selected, &auth.cwd);
                                        let supports_oauth = methods.oauth;
                                        let supports_api_key = methods.api_key;
                                        let primary_var = agent_auth::key_label(selected);
                                        let is_connected = auth.providers.iter().any(|p| p.id == *selected && p.ready);

                                        view.child(
                                            div()
                                                .mt(px(8.0))
                                                .p(px(14.0))
                                                .rounded(px(8.0))
                                                .bg(card_sub_bg)
                                                .border_1()
                                                .border_color(modal_divider)
                                                .flex()
                                                .flex_col()
                                                .gap(px(10.0))
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .justify_between()
                                                        .child(
                                                            div()
                                                                .flex()
                                                                .items_center()
                                                                .gap(px(8.0))
                                                                .child(
                                                                    div()
                                                                        .text_size(px(13.0))
                                                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                                                        .text_color(tokens.text_primary)
                                                                        .child(format!("Configure {provider_name}")),
                                                                )
                                                                .when(is_connected, |header| {
                                                                    header.child(
                                                                        div()
                                                                            .px(px(6.0))
                                                                            .py(px(1.5))
                                                                            .rounded(px(4.0))
                                                                            .bg(badge_success_bg)
                                                                            .text_size(px(10.5))
                                                                            .font_weight(gpui::FontWeight::MEDIUM)
                                                                            .text_color(tokens.block_success)
                                                                            .child("Connected"),
                                                                    )
                                                                }),
                                                        )
                                                        .child(
                                                            div()
                                                                .flex()
                                                                .items_center()
                                                                .gap(px(8.0))
                                                                .when(is_connected, |actions| {
                                                                    let disconnect_id = selected.clone();
                                                                    actions.child(
                                                                        div()
                                                                            .id(("card-disconnect-btn", pane_id))
                                                                            .px(px(8.0))
                                                                            .py(px(3.0))
                                                                            .rounded(px(4.0))
                                                                            .bg(disconnect_bg)
                                                                            .hover(|v| v.opacity(0.85))
                                                                            .text_color(tokens.block_failure)
                                                                            .text_size(px(11.0))
                                                                            .font_weight(gpui::FontWeight::MEDIUM)
                                                                            .cursor(CursorStyle::PointingHand)
                                                                            .child("Disconnect")
                                                                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                                                cx.stop_propagation();
                                                                                this.disconnect_agent_provider(pane_id, disconnect_id.clone(), window, cx);
                                                                            })),
                                                                    )
                                                                })
                                                                .child(
                                                                    div()
                                                                        .size(px(20.0))
                                                                        .rounded(px(4.0))
                                                                        .flex()
                                                                        .items_center()
                                                                        .justify_center()
                                                                        .cursor(CursorStyle::PointingHand)
                                                                        .hover(move |b| {
                                                                            b.bg(tokens.card_hover)
                                                                        })
                                                                        .child(
                                                                            svg()
                                                                                .path("icons/x.svg")
                                                                                .size(px(11.0))
                                                                                .text_color(tokens.text_muted),
                                                                        )
                                                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                                                            cx.stop_propagation();
                                                                            if let Some(auth) = this.pane_mut(pane_id).and_then(|p| p.chat.auth.as_mut()) {
                                                                                if let Some(mut login) = auth.login.take() {
                                                                                    login.cancel();
                                                                                }
                                                                                auth.selected = None;
                                                                                auth.output.clear();
                                                                                auth.error = None;
                                                                                cx.notify();
                                                                            }
                                                                        })),
                                                                ),
                                                        ),
                                                )
                                                .when(auth.providers.iter().any(|p| p.id == *selected && p.custom), |panel| {
                                                    let provider = selected.clone();
                                                    panel.child(div().id(("remove-custom-provider", pane_id))
                                                        .text_size(px(11.5)).text_color(tokens.block_failure)
                                                        .cursor(CursorStyle::PointingHand).child("Remove provider and models")
                                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                                            cx.stop_propagation(); this.remove_custom_provider(pane_id, provider.clone(), cx);
                                                        })))
                                                })
                                                .when(supports_oauth && auth.login.is_none(), |panel| {
                                                    let login_id = selected.clone();
                                                    panel.child(
                                                        div()
                                                            .id(("start-oauth-btn", pane_id))
                                                            .px(px(12.0))
                                                            .py(px(6.5))
                                                            .rounded(px(6.0))
                                                            .bg(tokens.primary_btn_bg)
                                                            .text_color(tokens.primary_btn_fg)
                                                            .text_size(px(12.0))
                                                            .font_weight(gpui::FontWeight::MEDIUM)
                                                            .cursor(CursorStyle::PointingHand)
                                                            .hover(|v| v.bg(tokens.primary_btn_hover))
                                                            .flex()
                                                            .items_center()
                                                            .justify_center()
                                                            .child(if is_connected { "Re-authenticate with OAuth" } else { "Connect with OAuth" })
                                                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                                cx.stop_propagation();
                                                                this.start_agent_oauth(pane_id, login_id.clone(), window, cx);
                                                            })),
                                                    )
                                                })
                                                .when(supports_api_key && auth.login.is_none(), |panel| {
                                                    panel.child(
                                                        div()
                                                            .flex()
                                                            .flex_col()
                                                            .gap(px(6.0))
                                                            .when(supports_oauth, |section| {
                                                                section.child(
                                                                    div()
                                                                        .flex()
                                                                        .items_center()
                                                                        .gap(px(8.0))
                                                                        .py(px(2.0))
                                                                        .child(
                                                                            div()
                                                                                .flex_1()
                                                                                .h(px(1.0))
                                                                                .bg(modal_divider),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .text_size(px(10.0))
                                                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                                                .text_color(tokens.text_muted)
                                                                                .child("OR ENTER API KEY"),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .flex_1()
                                                                                .h(px(1.0))
                                                                                .bg(modal_divider),
                                                                        ),
                                                                )
                                                            })
                                                            .child(
                                                                div()
                                                                    .text_size(px(11.5))
                                                                    .text_color(tokens.text_secondary)
                                                                    .child(format!("API Key ({primary_var}):")),
                                                            )
                                                            .child(
                                                                Input::new(&auth.input)
                                                                    .appearance(true)
                                                                    .bordered(true)
                                                                    .cleanable(true)
                                                                    .when(themed_input, |input| style_auth_input(input, tokens, credential_focused)),
                                                            )
                                                            .child(
                                                                div()
                                                                    .flex()
                                                                    .justify_end()
                                                                    .child(
                                                                        div()
                                                                            .id(("save-api-key-btn", pane_id))
                                                                            .px(px(12.0))
                                                                            .py(px(5.0))
                                                                            .rounded(px(5.0))
                                                                            .bg(btn_contrast_bg)
                                                                            .hover(move |v| {
                                                                                v.bg(btn_contrast_hover)
                                                                            })
                                                                            .text_color(tokens.text_primary)
                                                                            .text_size(px(11.5))
                                                                            .font_weight(gpui::FontWeight::MEDIUM)
                                                                            .cursor(CursorStyle::PointingHand)
                                                                            .child(if is_connected { "Update API Key" } else { "Save Key & Connect" })
                                                                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                                                cx.stop_propagation();
                                                                                this.submit_agent_auth(pane_id, window, cx);
                                                                            })),
                                                                    ),
                                                            ),
                                                    )
                                                })
                                                .when(auth.login.is_some(), |panel| {
                                                    panel.child(
                                                        div()
                                                            .flex()
                                                            .flex_col()
                                                            .gap(px(6.0))
                                                            .when(!auth.output.is_empty(), |out_view| {
                                                                out_view.child(
                                                                    div()
                                                                        .id(("auth-output", pane_id))
                                                                        .max_h(px(120.0))
                                                                        .overflow_y_scrollbar()
                                                                        .text_size(px(11.0))
                                                                        .font_family(MONO_FONT)
                                                                        .text_color(tokens.text_primary)
                                                                        .child(auth.output.clone()),
                                                                )
                                                            })
                                                            .child(
                                                                Input::new(&auth.input)
                                                                    .appearance(true)
                                                                    .bordered(true)
                                                                    .cleanable(true)
                                                                    .when(themed_input, |input| style_auth_input(input, tokens, credential_focused)),
                                                            )
                                                            .child(
                                                                div()
                                                                    .text_size(px(11.0))
                                                                    .text_color(tokens.text_muted)
                                                                    .child("Enter code or response if prompted, then press Enter."),
                                                            ),
                                                    )
                                                }),
                                        )
                                    })
                                    .when(auth.selected.is_none() && auth.custom_provider.is_none(), |view| {
                                        view.child(
                                            div()
                                                .mt(px(8.0))
                                                .pt(px(8.0))
                                                .border_t_1()
                                                .border_color(modal_divider)
                                                .flex()
                                                .flex_col()
                                                .gap(px(4.0))
                                                .child(
                                                    div().id(("add-compatible-provider", pane_id)).debug_selector(|| "add-compatible-provider".into())
                                                        .py(px(8.0)).text_color(tokens.text_primary)
                                                        .cursor(CursorStyle::PointingHand)
                                                        .child("+ OpenAI-compatible API")
                                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                            cx.stop_propagation();
                                                            this.open_custom_provider(pane_id, window, cx);
                                                        })),
                                                )
                                                .child(
                                                    div().debug_selector(|| "auth-provider-id-input".into())
                                                        .child(Input::new(&auth.provider_input)
                                                            .appearance(true)
                                                            .bordered(true)
                                                            .when(themed_input, |input| style_auth_input(input, tokens, provider_focused))),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(11.0))
                                                        .text_color(tokens.text_muted)
                                                        .child(
                                                            "Select a provider above, or enter an extension provider ID and press Enter.",
                                                        ),
                                                ),
                                        )
                                    })
                                }),
                        )
            .into_any_element()
    }
}
