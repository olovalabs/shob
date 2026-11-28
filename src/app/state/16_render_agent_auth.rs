impl AppState {
    fn render_agent_auth(
        &self,
        pane_id: usize,
        auth: &AuthModal,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tokens = self.tokens(cx);
        let filter_focus = auth.filter.read(cx).focus_handle().clone();
        let filter_focused = filter_focus.is_focused(window);
        let credential_focused = auth.input.read(cx).focus_handle().is_focused(window);
        let filter_query = auth.filter.read(cx).value().trim().to_lowercase();

        let columns = if window.viewport_size().width < px(480.0) {
            1
        } else {
            2
        };
        let tile_height = px(54.0);
        let tile_gap = px(8.0);
        let row_height = tile_height + tile_gap;
        let model_provider = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.chat.selected_model.as_ref())
            .map(|model| model.provider.as_str());

        let catalog = &self.agent_catalog;
        let mut providers: Vec<&agent_auth::Provider> = catalog
            .data
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
                provider.id.as_str(),
            )
        });
        let modal_divider = tokens.border_subtle;

        let background = tokens.card_bg;
        let foreground = tokens.text_primary;
        let accent = tokens.primary_btn_bg;
        let tile_bg = background.blend(foreground.opacity(0.035));
        let tile_border = tile_bg.blend(foreground.opacity(0.105));
        let tile_hover = background.blend(foreground.opacity(0.07));
        let selected_bg = background.blend(accent.opacity(0.11));
        let selected_hover = background.blend(accent.opacity(0.17));
        let icon_bg = background.blend(foreground.opacity(0.075));
        let selected_icon_bg = background.blend(accent.opacity(0.21));
        let disconnect_hover = background.blend(tokens.block_failure.opacity(0.16));

        let form_action = |id: &'static str, label: &'static str, primary, enabled| {
            let (bg, fg, hover, border) = if primary {
                (
                    accent,
                    tokens.primary_btn_fg,
                    tokens.primary_btn_hover,
                    accent,
                )
            } else {
                (
                    tokens.secondary_btn_bg,
                    tokens.secondary_btn_fg,
                    tokens.secondary_btn_hover,
                    tokens.card_border,
                )
            };
            div()
                .id((id, pane_id))
                .debug_selector(move || id.into())
                .role(gpui::Role::Button)
                .h(px(34.0))
                .px(px(12.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(border)
                .bg(bg)
                .text_color(fg)
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .flex()
                .items_center()
                .justify_center()
                .when(enabled, |button| {
                    button
                        .tab_index(0)
                        .cursor_pointer()
                        .hover(move |button| button.bg(hover))
                        .focus_visible(move |button| button.border_color(tokens.ring))
                })
                .when(!enabled, |button| button.opacity(0.65))
                .child(label)
        };

        struct ProviderCardItem {
            index: usize,
            id: String,
            name: SharedString,
            description: &'static str,
            ready: bool,
            current: bool,
            selected: bool,
            icon: SharedString,
        }

        let card_items: Vec<ProviderCardItem> = providers
            .iter()
            .enumerate()
            .map(|(index, provider)| {
                let (meta_name, description) = provider_metadata(&provider.id);
                let name = provider.name.as_deref().unwrap_or(if meta_name.is_empty() {
                    &provider.id
                } else {
                    meta_name
                });
                let current = provider.ready && model_provider == Some(provider.id.as_str());
                let selected = auth
                    .selected
                    .as_deref()
                    .map_or(current, |id| id == provider.id);
                ProviderCardItem {
                    index,
                    id: provider.id.clone(),
                    name: SharedString::from(name.to_owned()),
                    description,
                    ready: provider.ready,
                    current,
                    selected,
                    icon: picker_provider_icon(&provider.id, provider.custom),
                }
            })
            .collect();

        let result_count = card_items.len();
        let rows = card_items.len().div_ceil(columns);
        let visible_rows = rows.min(4);
        let scrollable = rows > visible_rows;
        let provider_sizes = Rc::new(vec![size(px(0.0), row_height); rows]);
        let provider_height = row_height * visible_rows as f32;
        let provider_list = v_virtual_list(
            cx.entity(),
            ("auth-provider-list", pane_id),
            provider_sizes,
            move |_, range, _, cx| {
                range
                    .map(|row_index| {
                        let start = row_index * columns;
                        let pair = &card_items[start..(start + columns).min(card_items.len())];
                        div()
                            .w_full()
                            .h(row_height)
                            .pb(tile_gap)
                            .when(scrollable, |row| row.pr(px(12.0)))
                            .flex()
                            .gap(tile_gap)
                            .children(pair.iter().map(|item| {
                                let index = item.index;
                                let id = item.id.clone();
                                let ready = item.ready;
                                let selected = item.selected;
                                let subtitle = if item.current {
                                    "Current model"
                                } else if ready {
                                    "Connected"
                                } else if selected {
                                    "Configuring"
                                } else {
                                    item.description
                                };

                                div()
                                    .id(("auth-provider", index))
                                    .debug_selector(move || format!("auth-provider-{index}"))
                                    .flex_1()
                                    .min_w_0()
                                    .h(tile_height)
                                    .px(px(12.0))
                                    .rounded(px(8.0))
                                    .border_1()
                                    .border_color(if selected { accent } else { tile_border })
                                    .bg(if selected { selected_bg } else { tile_bg })
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .cursor_pointer()
                                    .hover(move |tile| {
                                        tile.bg(if selected { selected_hover } else { tile_hover })
                                            .border_color(if selected {
                                                accent
                                            } else {
                                                tokens.card_border
                                            })
                                    })
                                    .child(
                                        div()
                                            .size(px(32.0))
                                            .flex_shrink_0()
                                            .rounded(px(8.0))
                                            .bg(if selected { selected_icon_bg } else { icon_bg })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                svg()
                                                    .path(item.icon.clone())
                                                    .size(px(17.0))
                                                    .text_color(foreground),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px(1.0))
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(13.0))
                                                    .line_height(px(17.0))
                                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                                    .child(item.name.clone()),
                                            )
                                            .when(!subtitle.is_empty(), |label| {
                                                label.child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(11.0))
                                                        .line_height(px(14.0))
                                                        .text_color(tokens.text_secondary)
                                                        .child(subtitle),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .id(("auth-provider-state", index))
                                            .debug_selector(move || {
                                                let state = if selected {
                                                    "selected"
                                                } else if ready {
                                                    "connected"
                                                } else {
                                                    "available"
                                                };
                                                format!("auth-provider-{state}-{index}")
                                            })
                                            .flex_shrink_0()
                                            .child(
                                                svg()
                                                    .path(if ready {
                                                        "icons/status-check.svg"
                                                    } else if selected {
                                                        "icons/chevron-down.svg"
                                                    } else {
                                                        "icons/chevron-right.svg"
                                                    })
                                                    .size(px(14.0))
                                                    .text_color(if ready {
                                                        tokens.block_success
                                                    } else {
                                                        tokens.text_secondary
                                                    }),
                                            ),
                                    )
                                    .when(ready, |tile| {
                                        let disconnect_id = id.clone();
                                        tile.child(
                                            div()
                                                .id(("auth-disconnect", index))
                                                .debug_selector(move || {
                                                    format!("auth-disconnect-{index}")
                                                })
                                                .size(px(28.0))
                                                .rounded(px(6.0))
                                                .flex()
                                                .flex_shrink_0()
                                                .items_center()
                                                .justify_center()
                                                .cursor_pointer()
                                                .hover(move |button| button.bg(disconnect_hover))
                                                .tooltip(|window, cx| {
                                                    gpui_component::tooltip::Tooltip::new(
                                                        "Disconnect provider",
                                                    )
                                                    .build(window, cx)
                                                })
                                                .child(
                                                    svg()
                                                        .path("icons/x.svg")
                                                        .size(px(12.0))
                                                        .text_color(tokens.text_secondary),
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
                                    )
                            }))
                            .when(pair.len() < columns, |row| {
                                row.child(div().flex_1().min_w_0())
                            })
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
                if !filter_focus.is_focused(window) {
                    this.submit_agent_auth(pane_id, window, cx);
                }
            }))
                        .when(auth.custom_provider.is_none(), |view| {
                            let search = style_auth_input(
                                Input::new(&auth.filter)
                                    .prefix(
                                        div()
                                            .pl(px(3.0))
                                            .pr(px(4.0))
                                            .flex()
                                            .items_center()
                                            .child(
                                                svg()
                                                    .path("ui_icons/search.svg")
                                                    .size(px(15.0))
                                                    .text_color(tokens.text_secondary),
                                            ),
                                    )
                                    .cleanable(true),
                                tokens,
                                filter_focused,
                            )
                            .rounded(px(8.0));
                            view.child(
                                div()
                                    .id(("auth-filter", pane_id))
                                    .debug_selector(|| "agent-dialog-filter".into())
                                    .mx(px(20.0))
                                    .mb(px(6.0))
                                    .flex_shrink_0()
                                    .child(gpui::Styled::h(search, px(38.0))),
                            )
                        })
                        .when(auth.custom_provider.is_some(), |view| {
                            view.child(div().h(px(1.0)).bg(modal_divider).flex_shrink_0())
                        })
                        .child(
                            div()
                                .id(("auth-provider-body", pane_id))
                                .px(px(20.0))
                                .py(px(12.0))
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scrollbar()
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .when_some(auth.custom_provider.as_ref(), |view, fields| {
                                    let labels = ["ID", "Name", "Endpoint URL", "API key", "Models"];
                                    let saving = auth.provider_operation.is_some();
                                    let mut form = div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(10.0))
                                        .child(
                                            div().flex().flex_col().gap(px(4.0))
                                                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("OpenAI-compatible API"))
                                                .child(div().text_size(px(12.0)).text_color(tokens.text_secondary)
                                                    .child("Use a Chat Completions base URL, including /v1 when required. Enter exact model IDs separated by commas.")),
                                        );
                                    for (label, field) in labels.into_iter().zip(fields) {
                                        let focused = field.read(cx).focus_handle().is_focused(window);
                                        form = form.child(
                                            div().flex().flex_col().gap(px(4.0))
                                                .child(div().text_size(px(12.0)).text_color(tokens.text_secondary).child(label))
                                                .child(style_auth_input(Input::new(field), tokens, focused)),
                                        );
                                    }
                                    view.child(
                                        form
                                            .when_some(auth.error.as_ref(), |form, error| {
                                                form.child(div().text_color(tokens.block_failure).child(error.clone()))
                                            })
                                            .child(
                                                div().mt(px(4.0)).flex().justify_end().gap(px(8.0))
                                                    .child(
                                                        form_action("cancel-compatible-provider", "Cancel", false, !saving)
                                                            .when(!saving, |button| button.on_click(cx.listener(move |this, _, window, cx| {
                                                                if let Some(auth) = this.pane_mut(pane_id).and_then(|p| p.chat.auth.as_mut())
                                                                    && auth.provider_operation.is_none()
                                                                {
                                                                    auth.custom_provider = None;
                                                                    auth.error = None;
                                                                    auth.filter.update(cx, |state, cx| state.focus(window, cx));
                                                                    cx.notify();
                                                                }
                                                            }))),
                                                    )
                                                    .child(
                                                        form_action(
                                                            "save-compatible-provider",
                                                            if saving { "Saving…" } else { "Save & Connect" },
                                                            true,
                                                            !saving,
                                                        )
                                                        .when(!saving, |button| button.on_click(cx.listener(move |this, _, _, cx| {
                                                            this.save_custom_provider(pane_id, cx);
                                                        }))),
                                                    ),
                                            ),
                                    )
                                })
                                .when(auth.page == AgentDialogPage::Auth && auth.custom_provider.is_none(), |view| {
                                    view.when(!filter_query.is_empty() && result_count > 0, |view| {
                                        view.child(
                                            div()
                                                .pb(px(4.0))
                                                .text_size(px(12.0))
                                                .text_color(tokens.text_secondary)
                                                .child(if result_count == 1 {
                                                    "1 provider found".to_owned()
                                                } else {
                                                    format!("{result_count} providers found")
                                                }),
                                        )
                                    })
                                    .when(catalog.pending.is_some() && catalog.data.providers.is_empty(), |view| {
                                        view.child(
                                            div()
                                                .py(px(8.0))
                                                .text_size(px(12.0))
                                                .text_color(tokens.text_muted)
                                                .child("Checking providers…"),
                                        )
                                    })
                                    .when(result_count > 0, |view| {
                                        view.child(
                                            div()
                                                .id(("auth-providers", pane_id))
                                                .relative()
                                                .flex_shrink_0()
                                                .h(provider_height)
                                                .child(provider_list)
                                                .when(scrollable, |list| {
                                                    list.child(
                                                        KitScrollbar::vertical(&auth.provider_scroll)
                                                            .mode(gpui_kit::base::ScrollbarMode::Always),
                                                    )
                                                }),
                                        )
                                    })
                                    .when(
                                        !filter_query.is_empty()
                                            && result_count == 0
                                            && !catalog.data.providers.is_empty(),
                                        |view| {
                                            view.child(
                                                div()
                                                    .py(px(20.0))
                                                    .px(px(12.0))
                                                    .rounded(px(8.0))
                                                    .bg(tile_bg)
                                                    .border_1()
                                                    .border_color(tile_border)
                                                    .flex()
                                                    .flex_col()
                                                    .items_center()
                                                    .gap(px(4.0))
                                                    .child(
                                                        div()
                                                            .text_size(px(13.0))
                                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                                            .child("No matching providers"),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_size(px(12.0))
                                                            .text_color(tokens.text_secondary)
                                                            .child("Try a provider name or ID."),
                                                    ),
                                            )
                                        },
                                    )
                                    .when(
                                        catalog.pending.is_none() && catalog.data.providers.is_empty(),
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
                                                                auth.discover_auth_methods();
                                                                this.agent_catalog.refresh();
                                                                cx.notify();
                                                            }
                                                        }),
                                                    ),
                                            )
                                        },
                                    )
                                    .when_some(auth.error.as_ref().or(catalog.error.as_ref()), |view, error| {
                                        view.child(
                                            div()
                                                .py(px(4.0))
                                                .text_size(px(11.5))
                                                .text_color(tokens.block_failure)
                                                .child(error.clone()),
                                        )
                                    })
                                    .when_some(auth.selected.as_ref(), |view, selected| {
                                        let provider_name = catalog.provider_name(selected);
                                        let methods = agent_auth::auth_methods(selected, &auth.cwd);
                                        let supports_oauth = methods.oauth;
                                        let supports_api_key = methods.api_key;
                                        let primary_var = agent_auth::key_label(selected);
                                        let is_connected = catalog.data.providers.iter().any(|p| p.id == *selected && p.ready);

                                        view.child(
                                            div()
                                                .mt(px(8.0))
                                                .p(px(14.0))
                                                .rounded(px(8.0))
                                                .bg(tokens.card_hover)
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
                                                                            .flex()
                                                                            .items_center()
                                                                            .gap(px(4.0))
                                                                            .text_size(px(11.0))
                                                                            .text_color(tokens.text_secondary)
                                                                            .child(svg().path("icons/status-check.svg").size(px(12.0)).text_color(tokens.text_secondary))
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
                                                                            .hover(move |button| button.bg(tokens.pane_header_btn_hover))
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
                                                .when(catalog.data.providers.iter().any(|p| p.id == *selected && p.custom), |panel| {
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
                                                        form_action(
                                                            "start-oauth-btn",
                                                            if is_connected { "Re-authenticate with OAuth" } else { "Connect with OAuth" },
                                                            true,
                                                            true,
                                                        )
                                                        .on_click(cx.listener(move |this, _, window, cx| {
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
                                                                style_auth_input(Input::new(&auth.input).cleanable(true), tokens, credential_focused),
                                                            )
                                                            .child(
                                                                div()
                                                                    .flex()
                                                                    .justify_end()
                                                                    .child(
                                                                        form_action(
                                                                            "save-api-key-btn",
                                                                            if is_connected { "Update API Key" } else { "Save Key & Connect" },
                                                                            !supports_oauth,
                                                                            true,
                                                                        )
                                                                        .on_click(cx.listener(move |this, _, window, cx| {
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
                                                                style_auth_input(Input::new(&auth.input).cleanable(true), tokens, credential_focused),
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
                                    .when(auth.selected.is_none(), |view| {
                                        view.child(
                                            div()
                                                .mt(px(4.0))
                                                .pt(px(12.0))
                                                .border_t_1()
                                                .border_color(modal_divider)
                                                .flex()
                                                .justify_center()
                                                .child(
                                                    form_action("add-compatible-provider", "+ Add OpenAI-compatible provider", false, true)
                                                        .on_click(cx.listener(move |this, _, window, cx| {
                                                            this.open_custom_provider(pane_id, window, cx);
                                                        })),
                                                ),
                                        )
                                    })
                                }),
                        )
            .into_any_element()
    }
}
