impl AppState {
    /// Window-level host for the provider/model dialog. Rendering it at the
    /// root (instead of inside a pane) lets its backdrop cover the sidebar and
    /// title bar, so nothing behind it receives clicks, hover or scroll.
    fn render_agent_auth_layer(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(pane_id) = self.open_agent_auth_pane() else {
            return div().into_any_element();
        };
        let Some(auth) = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.chat.auth.as_ref())
        else {
            return div().into_any_element();
        };
        let tokens = self.tokens(cx);
        let page = auth.page;
        let pane = self.pane_ref(pane_id).unwrap();
        let trigger = pane.model_picker_bounds.get();
        let viewport = window.viewport_size();
        let anchored =
            page == AgentDialogPage::Models && pane.chat.active && trigger.size.width > px(0.0);
        let picker_width = px(554.0).min((viewport.width - px(16.0)).max(px(0.0)));
        let above = trigger.top() - px(12.0);
        let below = viewport.height - trigger.bottom() - px(12.0);
        let open_above = above >= px(393.0) || above >= below;
        let available_height = if anchored {
            if open_above { above } else { below }
        } else {
            viewport.height - px(16.0)
        };
        let picker_height = px(393.0).min(available_height.max(px(0.0)));
        let picker_top = if open_above {
            trigger.top() - picker_height - px(4.0)
        } else {
            trigger.bottom() + px(4.0)
        };
        let max_left = (viewport.width - picker_width - px(8.0)).max(px(8.0));
        let picker_left = (trigger.right() - picker_width).clamp(px(8.0), max_left);
        let content = match page {
            AgentDialogPage::Models => {
                self.render_agent_models(pane_id, auth, picker_width, picker_height, window, cx)
            }
            AgentDialogPage::Auth => self.render_agent_auth(pane_id, auth, window, cx),
        };
        deferred(
            div()
                .id(("auth-layer", pane_id))
                .debug_selector(|| "agent-auth-layer".into())
                .absolute()
                .size_full()
                .top_0()
                .left_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .when(!anchored, |layer| layer.bg(rgba(0x000000a6)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_agent_auth(pane_id, window, cx);
                    }),
                )
                .child(
                    div()
                        .id(("auth-panel", pane_id))
                        .debug_selector(move || {
                            if page == AgentDialogPage::Models {
                                "agent-model-panel".into()
                            } else {
                                "agent-provider-panel".into()
                            }
                        })
                        .track_focus(&auth.focus)
                        .when(anchored, |panel| {
                            panel
                                .absolute()
                                .left(picker_left)
                                .top(picker_top)
                                .shadow_lg()
                        })
                        .when(page == AgentDialogPage::Models, |panel| {
                            panel.w(picker_width).h(picker_height)
                        })
                        .when(page == AgentDialogPage::Auth, |panel| panel.w(px(520.0)))
                        .max_w(relative(0.97))
                        .max_h(relative(0.98))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(tokens.card_border)
                        .bg(if page == AgentDialogPage::Models {
                            tokens.model_picker_bg
                        } else {
                            tokens.card_bg
                        })
                        .text_color(tokens.text_primary)
                        .text_size(px(14.0))
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" {
                                cx.stop_propagation();
                                this.close_agent_auth(pane_id, window, cx);
                            }
                        }))
                        .when(page == AgentDialogPage::Auth, |panel| {
                            panel.child(
                                div()
                                    .h(px(48.0))
                                    .flex_shrink_0()
                                    .px(px(20.0))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.0))
                                            .when(page == AgentDialogPage::Auth, |header| {
                                                header.child(
                                                    div()
                                                        .id(("models-tab", pane_id))
                                                        .debug_selector(|| {
                                                            "agent-model-back".into()
                                                        })
                                                        .size(px(24.0))
                                                        .rounded(px(4.0))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .cursor(CursorStyle::PointingHand)
                                                        .hover(move |view| {
                                                            view.bg(tokens.card_hover)
                                                        })
                                                        .child(
                                                            svg()
                                                                .path("icons/arrow-left.svg")
                                                                .size(px(14.0))
                                                                .text_color(tokens.text_muted),
                                                        )
                                                        .on_mouse_down(
                                                            MouseButton::Left,
                                                            cx.listener(
                                                                move |this, _, window, cx| {
                                                                    cx.stop_propagation();
                                                                    this.set_agent_dialog_page(
                                                                        pane_id,
                                                                        AgentDialogPage::Models,
                                                                        window,
                                                                        cx,
                                                                    );
                                                                },
                                                            ),
                                                        ),
                                                )
                                            })
                                            .child(div().text_size(px(15.0)).child(match page {
                                                AgentDialogPage::Models => "Select model",
                                                AgentDialogPage::Auth => "Manage providers",
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id(("auth-close", pane_id))
                                            .debug_selector(|| "agent-dialog-close".into())
                                            .size(px(24.0))
                                            .rounded(px(4.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor(CursorStyle::PointingHand)
                                            .hover(move |view| view.bg(tokens.card_hover))
                                            .child(
                                                svg()
                                                    .path("icons/x.svg")
                                                    .size(px(14.0))
                                                    .text_color(tokens.text_muted),
                                            )
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.close_agent_auth(pane_id, window, cx);
                                                }),
                                            ),
                                    ),
                            )
                        })
                        .child(content),
                ),
        )
        .priority(210)
        .into_any_element()
    }

    fn render_agent_models(
        &self,
        pane_id: usize,
        auth: &AuthModal,
        width: Pixels,
        height: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = self.tokens(cx);
        let query = auth.filter.read(cx).value().trim().to_lowercase();
        let mut row_hover = tokens.text_primary;
        row_hover.a = 0.08;
        let mut footer_tint = tokens.text_primary;
        footer_tint.a = 0.012;
        let pane = self.pane_ref(pane_id).unwrap();
        let chat = &pane.chat;
        let selected = chat
            .selected_model
            .as_ref()
            .filter(|model| agent_auth::provider_visible_in_chat_ui(&model.provider));
        let mut providers: Vec<_> = auth
            .providers
            .iter()
            .filter(|p| p.ready && agent_auth::provider_visible_in_chat_ui(&p.id))
            .map(|p| p.id.clone())
            .collect();
        providers.sort();
        providers.dedup();
        let filter = match &auth.model_filter {
            ModelPickerFilter::Provider(provider) if !providers.contains(provider) => {
                &ModelPickerFilter::All
            }
            filter => filter,
        };
        let reasoning_model = selected.filter(|model| match filter {
            ModelPickerFilter::Provider(provider) => provider == &model.provider,
            _ => true,
        });
        let favorites = &self.settings.favorite_agent_models;
        let has_connected_providers = !providers.is_empty();
        let mut models: Vec<_> = auth
            .models
            .iter()
            .filter(|model| {
                providers.contains(&model.provider)
                    && match filter {
                        ModelPickerFilter::All => true,
                        ModelPickerFilter::Favorites => favorites.contains(model),
                        ModelPickerFilter::Provider(provider) => provider == &model.provider,
                    }
                    && (query.is_empty()
                        || model.id.to_lowercase().contains(&query)
                        || model.provider.to_lowercase().contains(&query)
                        || auth
                            .provider_name(&model.provider)
                            .to_lowercase()
                            .contains(&query))
            })
            .cloned()
            .collect();
        let starred_first = *filter == ModelPickerFilter::All && !favorites.is_empty();
        let starred: std::collections::HashSet<_> =
            favorites.iter().filter(|_| starred_first).collect();
        models.sort_by(|a, b| {
            (!starred.contains(a), &a.provider, &a.id).cmp(&(
                !starred.contains(b),
                &b.provider,
                &b.id,
            ))
        });
        let empty = models.is_empty();
        let list_height = height - px(2.0 + 44.0 + 51.0);
        let scrollable = px(34.0 * models.len() as f32) > list_height;
        let loading = auth.discovery.is_some() || auth.model_discovery.is_some();
        let selected_model = selected.cloned();
        let shortcuts: Vec<_> = models.iter().take(9).cloned().collect();
        let shortcut_modifier = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl+"
        };
        let item_sizes = Rc::new(models.iter().map(|_| size(px(0.0), px(34.0))).collect());
        let favorites = favorites.clone();
        let list = v_virtual_list(
            cx.entity(),
            ("agent-model-list", pane_id),
            item_sizes,
            move |_, range, _, cx| {
                range
                    .map(|index| {
                        let model = &models[index];
                        let choice = model.clone();
                        let is_selected = selected_model.as_ref() == Some(model);
                        let favorite = favorites.contains(model);
                        let favorite_model = choice.clone();
                        let keyboard_favorite_model = choice.clone();
                        div()
                            .id(("agent-model", index))
                            .debug_selector(move || format!("agent-model-{index}"))
                            .w_full()
                            .h(px(34.0))
                            .px(px(8.0))
                            .group("picker-model-row")
                            .rounded(px(4.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .cursor(CursorStyle::PointingHand)
                            .hover(move |row| row.bg(row_hover))
                            .child(
                                svg()
                                    .path(provider_icon_path(&model.provider))
                                    .size(px(15.0))
                                    .flex_shrink_0()
                                    .text_color(tokens.text_primary),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(div().min_w_0().truncate().child(model.id.clone()))
                                    .when(is_selected, |name| {
                                        name.child(
                                            svg()
                                                .path("icons/status-check.svg")
                                                .size(px(12.0))
                                                .flex_shrink_0()
                                                .text_color(tokens.text_muted),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .id(("favorite-model", index))
                                    .debug_selector(move || format!("favorite-model-{index}"))
                                    .size(px(17.0))
                                    .tab_index(0)
                                    .cursor_pointer()
                                    .when(!favorite, |button| button.opacity(0.0))
                                    .group_hover("picker-model-row", |button| button.opacity(1.0))
                                    .focus_visible(move |button| button.opacity(1.0).bg(row_hover))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .text_color(if favorite {
                                        tokens.text_primary
                                    } else {
                                        tokens.text_muted
                                    })
                                    .hover(move |button| button.bg(tokens.pane_header_btn_hover))
                                    .tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(if favorite {
                                            "Remove from favorites"
                                        } else {
                                            "Add to favorites"
                                        })
                                        .build(window, cx)
                                    })
                                    .child(if favorite { "★" } else { "☆" })
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, _, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                cx.stop_propagation();
                                                this.toggle_favorite_model(
                                                    &keyboard_favorite_model,
                                                    cx,
                                                );
                                            }
                                        },
                                    ))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.toggle_favorite_model(&favorite_model, cx);
                                        }),
                                    ),
                            )
                            .when(index < 9, |row| {
                                row.child(
                                    div()
                                        .flex_shrink_0()
                                        .px(px(4.0))
                                        .rounded(px(4.0))
                                        .bg(row_hover)
                                        .text_size(px(12.0))
                                        .text_color(tokens.text_muted)
                                        .child(format!("{shortcut_modifier}{}", index + 1)),
                                )
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.select_agent_model(
                                        pane_id,
                                        Some(choice.clone()),
                                        window,
                                        cx,
                                    );
                                }),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&auth.model_scroll)
        .px(px(12.0));
        let filter_focused = auth.filter.read(cx).focus_handle().is_focused(window);
        let levels = reasoning_model
            .map(|model| model.thinking_levels())
            .unwrap_or_default();
        let current = PaneChat::resolve_thinking_level(chat.mode_thinking_level(), &levels);
        let default = (!levels.is_empty()).then(|| {
            PaneChat::resolve_thinking_level(if chat.fast_mode { "low" } else { "high" }, &levels)
        });
        let reasoning_provider = reasoning_model.map(|model| auth.provider_name(&model.provider));
        let reasoning_tooltip = reasoning_model
            .zip(reasoning_provider.as_ref())
            .map(|(model, provider)| format!("{provider} · {}", model.id));
        let mut rail = div()
            .id(("model-provider-rail", pane_id))
            .overflow_y_scrollbar()
            .w(px(45.0))
            .flex_shrink_0()
            .border_r_1()
            .border_color(tokens.card_border)
            .flex()
            .flex_col();
        rail = rail
            .child(
                div()
                    .id(("model-picker-favorites", pane_id))
                    .debug_selector(|| "model-picker-favorites".into())
                    .relative()
                    .h(px(44.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_size(px(21.0))
                    .when(*filter == ModelPickerFilter::Favorites, |tab| {
                        tab.child(model_picker_filter_marker(tokens.model_picker_accent, 44.0))
                    })
                    .hover(move |tab| tab.bg(row_hover))
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("Favorites").build(window, cx)
                    })
                    .child("★")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(auth) = this
                                .pane_mut(pane_id)
                                .and_then(|pane| pane.chat.auth.as_mut())
                            {
                                auth.filter_models(ModelPickerFilter::Favorites);
                                cx.notify();
                            }
                        }),
                    ),
            )
            .child(
                div()
                    .mx(px(4.0))
                    .h(px(1.0))
                    .flex_shrink_0()
                    .bg(tokens.border_subtle),
            );
        for provider in &providers {
            let active = matches!(filter, ModelPickerFilter::Provider(id) if id == provider);
            let choice = provider.clone();
            let name = auth.provider_name(provider);
            let selector = format!("model-provider-filter-{provider}");
            rail = rail.child(
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .relative()
                    .h(px(40.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor(CursorStyle::PointingHand)
                    .when(active, |tab| {
                        tab.child(model_picker_filter_marker(tokens.model_picker_accent, 40.0))
                    })
                    .hover(move |tab| tab.bg(row_hover))
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(name.clone()).build(window, cx)
                    })
                    .child(
                        svg()
                            .path(provider_icon_path(provider))
                            .size(px(18.0))
                            .text_color(if active {
                                tokens.text_primary
                            } else {
                                tokens.text_muted
                            }),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(auth) = this
                                .pane_mut(pane_id)
                                .and_then(|pane| pane.chat.auth.as_mut())
                            {
                                auth.filter_models(ModelPickerFilter::Provider(choice.clone()));
                                cx.notify();
                            }
                        }),
                    ),
            );
        }
        let reasoning = div()
            .w(px(192.0).min(width * 0.39))
            .id(("picker-reasoning", pane_id))
            .overflow_y_scrollbar()
            .flex_shrink_0()
            .border_l_1()
            .border_color(tokens.border_subtle)
            .flex()
            .flex_col()
            .px(px(12.0))
            .child(
                div()
                    .id(("picker-reasoning-header", pane_id))
                    .h(px(42.0))
                    .mb(px(10.0))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(tokens.border_subtle)
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .text_color(tokens.text_secondary)
                    .child(div().truncate().child("Reasoning"))
                    .when_some(reasoning_provider, |header, provider| {
                        header.child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(px(12.0))
                                .debug_selector({
                                    let provider = provider.clone();
                                    move || format!("picker-reasoning-provider-{provider}")
                                })
                                .child(provider),
                        )
                    })
                    .when_some(reasoning_tooltip, |header, tooltip| {
                        header.tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
                        })
                    }),
            )
            .when(reasoning_model.is_none(), |panel| {
                panel.child(
                    div()
                        .px(px(4.0))
                        .text_size(px(12.0))
                        .text_color(tokens.text_muted)
                        .child("Select a model to configure reasoning."),
                )
            })
            .children(levels.iter().map(|level| {
                let active = current == level.as_str();
                let is_default = default == Some(level.as_str());
                let label = match level.as_str() {
                    "off" | "none" => "None",
                    "low" => "Low",
                    "max" => "Max",
                    level => reasoning_effort_label(level),
                }
                .to_owned();
                let level = level.clone();
                div()
                    .id(SharedString::from(format!("model-reasoning-{level}")))
                    .debug_selector({
                        let level = level.clone();
                        move || format!("model-reasoning-{level}")
                    })
                    .h(px(34.0))
                    .flex_shrink_0()
                    .px(px(4.0))
                    .rounded(px(4.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .cursor(CursorStyle::PointingHand)
                    .hover(move |v| v.bg(row_hover))
                    .child(label)
                    .when(is_default, |row| {
                        row.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(tokens.text_muted)
                                .child("Default"),
                        )
                    })
                    .when(active && !is_default, |v| {
                        v.child(
                            svg()
                                .path("icons/status-check.svg")
                                .size(px(12.0))
                                .text_color(tokens.text_muted),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.set_reasoning_effort(pane_id, &level, cx);
                        }),
                    )
            }));
        let mut fade_start = tokens.model_picker_bg;
        fade_start.a = 0.0;
        div().relative().flex().flex_col().tab_group().h((height - px(2.0)).max(px(0.0))).min_h_0()
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let key = &event.keystroke;
                let shortcut = if cfg!(target_os = "macos") { key.modifiers.platform } else { key.modifiers.control };
                if shortcut && !key.modifiers.alt && !key.modifiers.shift
                    && let Ok(number) = key.key.parse::<usize>()
                    && let Some(model) = number.checked_sub(1).and_then(|index| shortcuts.get(index))
                {
                    cx.stop_propagation();
                    this.select_agent_model(pane_id, Some(model.clone()), window, cx);
                }
            }))
            .child(div().flex_1().min_h_0().flex().child(rail)
                .child(div().flex_1().min_w_0().flex().flex_col()
                    .child(div().id(("model-filter", pane_id)).debug_selector(|| "agent-model-filter".into()).mx(px(12.0)).mb(px(9.0)).h(px(42.0)).flex_shrink_0().flex().items_center().border_b_1()
                        .border_color(if filter_focused { tokens.model_picker_accent } else { tokens.border_subtle })
                        .child(Input::new(&auth.filter).w_full().text_size(px(14.0)).appearance(false).bordered(false).focus_bordered(false)
                            .prefix(svg().path("ui_icons/search.svg").size(px(14.0)).text_color(tokens.text_muted))))
                    .child(div().id(("agent-models", pane_id)).relative().flex_1().min_h_0()
                        .when(!empty, |v| v.child(list)
                            .when(scrollable, |list| {
                                list.child(
                                    div().absolute().left_0().right_0().bottom_0().h(px(17.0))
                                        .bg(gpui::linear_gradient(
                                            180.0,
                                            gpui::linear_color_stop(fade_start, 0.0),
                                            gpui::linear_color_stop(tokens.model_picker_bg, 1.0),
                                        )),
                                )
                            })
                            .child(KitScrollbar::vertical(&auth.model_scroll)))
                        .when(empty, |v| v.child(div().px(px(12.0)).py(px(16.0)).text_size(px(12.0)).text_color(tokens.text_muted).child(if loading { "Loading models…" } else if !query.is_empty() { "No models match your search." } else if !has_connected_providers { "No providers connected. Choose Manage to connect one." } else if *filter == ModelPickerFilter::Favorites { "No favorites yet. Star a model to add it here." } else { "No models available." }))))
                    .when_some(auth.model_error.as_ref().or(auth.error.as_ref()), |v, error| v.child(div().px(px(12.0)).text_color(tokens.block_failure).child(error.clone()))))
                .child(reasoning))
            .child(div().h(px(44.0)).flex_shrink_0().bg(footer_tint).border_t_1().border_color(tokens.border_subtle).px(px(12.0)).flex().items_center().gap(px(10.0)).text_size(px(12.0)).text_color(tokens.text_secondary)
                .child(
                    div()
                        .id(("models-open-providers", pane_id))
                        .debug_selector(|| "agent-more-providers".into())
                        .cursor_pointer()
                        .hover(move |v| v.text_color(tokens.text_primary))
                        .child("Manage")
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.set_agent_dialog_page(pane_id, AgentDialogPage::Auth, window, cx);
                        })),
                )
                .child(div().flex_1())
                .child(self.render_agent_permission(pane, true, window, cx)))
            .into_any_element()
    }
}
