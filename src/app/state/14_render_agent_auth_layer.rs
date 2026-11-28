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
        let backdrop = cx.theme().overlay;
        let page = auth.page;
        let pane = self.pane_ref(pane_id).unwrap();
        let trigger = pane.model_picker_bounds.get();
        let viewport = window.viewport_size();
        let anchored = self.bot_form.is_none()
            && page == AgentDialogPage::Models
            && pane.chat.active
            && trigger.size.width > px(0.0);
        let picker_width = px(420.0).min((viewport.width - px(16.0)).max(px(0.0)));
        let preferred_height = px(436.0);
        let above = trigger.top() - px(12.0);
        let below = viewport.height - trigger.bottom() - px(12.0);
        let open_above = above >= preferred_height || above >= below;
        let available_height = if anchored {
            if open_above { above } else { below }
        } else {
            viewport.height - px(16.0)
        };
        let picker_height = preferred_height.min(available_height.max(px(0.0)));
        let picker_top = if open_above {
            trigger.top() - picker_height - px(4.0)
        } else {
            trigger.bottom() + px(4.0)
        };
        let max_left = (viewport.width - picker_width - px(8.0)).max(px(8.0));
        let picker_left = (trigger.right() - picker_width).clamp(px(8.0), max_left);
        let content = match page {
            AgentDialogPage::Models => {
                self.render_agent_models(pane_id, auth, picker_height, window, cx)
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
                .when(!anchored, |layer| layer.bg(backdrop))
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
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(if page == AgentDialogPage::Models {
                            tokens
                                .model_picker_bg
                                .blend(tokens.model_picker_accent.opacity(0.24))
                        } else {
                            tokens.card_border
                        })
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
        height: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = self.tokens(cx);
        let query = auth.filter.read(cx).value().trim().to_lowercase();
        let accent = tokens.model_picker_accent;
        let surface = tokens.model_picker_bg;
        let row_hover = tokens.text_primary.opacity(0.08);
        let active_row = tokens.text_primary.opacity(0.11);
        let chip = tokens.text_primary.opacity(0.055);
        let compact = height < px(320.0);
        let search_height = px(if compact { 36.0 } else { 44.0 });
        let rail_height = px(if compact { 36.0 } else { 48.0 });
        let footer_height = px(if compact { 40.0 } else { 52.0 });
        let reasoning_height = px(if compact { 54.0 } else { 76.0 });
        let row_height = px(36.0);
        let pane = self.pane_ref(pane_id).unwrap();
        let chat = &pane.chat;
        let selected = chat
            .selected_model
            .as_ref()
            .filter(|model| agent_auth::provider_visible_in_chat_ui(&model.provider));
        let catalog = &self.agent_catalog;
        let mut providers: Vec<_> = catalog
            .data
            .providers
            .iter()
            .filter(|p| p.ready && agent_auth::provider_visible_in_chat_ui(&p.id))
            .collect();
        providers.sort_by_key(|p| (provider_featured_rank(&p.id), p.id.as_str()));
        providers.dedup_by(|a, b| a.id == b.id);
        let provider_ids: std::collections::HashSet<_> =
            providers.iter().map(|p| p.id.as_str()).collect();
        let filter = match &auth.model_filter {
            ModelPickerFilter::Provider(provider) if !provider_ids.contains(provider.as_str()) => {
                &ModelPickerFilter::All
            }
            filter => filter,
        };
        let favorites = &self.settings.favorite_agent_models;
        let has_connected_providers = !providers.is_empty();
        let mut models: Vec<_> = catalog
            .data
            .models
            .iter()
            .filter(|model| {
                provider_ids.contains(model.provider.as_str())
                    && match filter {
                        ModelPickerFilter::All => true,
                        ModelPickerFilter::Favorites => favorites.contains(model),
                        ModelPickerFilter::Provider(provider) => provider == &model.provider,
                    }
                    && (query.is_empty()
                        || model.id.to_lowercase().contains(&query)
                        || model.provider.to_lowercase().contains(&query)
                        || catalog
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
        let loading = !catalog.loaded && catalog.pending.is_some();
        let selected_model = selected.cloned();
        let shortcuts: Vec<_> = models.iter().take(9).cloned().collect();
        let item_sizes = Rc::new(models.iter().map(|_| size(px(0.0), row_height)).collect());
        let provider_icons: std::collections::HashMap<_, _> = providers
            .iter()
            .map(|p| (p.id.clone(), picker_provider_icon(&p.id, p.custom)))
            .collect();
        let row_icons = provider_icons.clone();
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
                            .h(row_height)
                            .px(px(8.0))
                            .group("picker-model-row")
                            .rounded(px(5.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .cursor(CursorStyle::PointingHand)
                            .when(is_selected, |row| row.bg(active_row))
                            .hover(move |row| {
                                row.bg(if is_selected { active_row } else { row_hover })
                            })
                            .when_some(row_icons.get(&model.provider), |row, icon| {
                                row.child(
                                    svg()
                                        .path(icon.clone())
                                        .size(px(20.0))
                                        .flex_shrink_0()
                                        .text_color(tokens.text_primary),
                                )
                            })
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
                                                .size(px(13.0))
                                                .flex_shrink_0()
                                                .text_color(tokens.text_secondary),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .id(("favorite-model", index))
                                    .debug_selector(move || format!("favorite-model-{index}"))
                                    .size(px(26.0))
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
                                        accent
                                    } else {
                                        tokens.text_secondary
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
        .px(px(13.0));
        let filter_focused = auth.filter.read(cx).focus_handle().is_focused(window);
        let tab_size = px(if compact { 29.0 } else { 35.0 });
        let filter_tab = |active| {
            div()
                .size(tab_size)
                .flex_shrink_0()
                .rounded(px(7.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .when(active, |tab| tab.bg(accent.opacity(0.18)))
        };
        let favorites_active = *filter == ModelPickerFilter::Favorites;
        let rail = div()
            .id(("model-provider-rail", pane_id))
            .debug_selector(|| "model-provider-rail".into())
            .h(rail_height)
            .px(px(9.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .overflow_x_scroll()
            .child(
                filter_tab(favorites_active)
                    .id(("model-picker-favorites", pane_id))
                    .debug_selector(|| "model-picker-favorites".into())
                    .text_size(px(23.0))
                    .text_color(accent)
                    .hover(move |tab| tab.bg(accent.opacity(0.24)))
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(if favorites_active {
                            "Show all models"
                        } else {
                            "Favorites"
                        })
                        .build(window, cx)
                    })
                    .child("★")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(auth) =
                                this.pane_mut(pane_id).and_then(|p| p.chat.auth.as_mut())
                            {
                                auth.filter_models(ModelPickerFilter::Favorites);
                                cx.notify();
                            }
                        }),
                    ),
            )
            .children(providers.iter().filter_map(|provider| {
                let choice = provider.id.clone();
                let icon = provider_icons.get(&choice)?;
                let active = matches!(filter, ModelPickerFilter::Provider(id) if id == &choice);
                let name = catalog.provider_name(&choice);
                let selector = format!("model-provider-filter-{choice}");
                Some(
                    filter_tab(active)
                        .id(SharedString::from(selector.clone()))
                        .debug_selector(move || selector.clone())
                        .hover(move |tab| {
                            tab.bg(if active {
                                accent.opacity(0.24)
                            } else {
                                row_hover
                            })
                        })
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(name.clone()).build(window, cx)
                        })
                        .child(
                            svg()
                                .path(icon.clone())
                                .size(px(24.0))
                                .text_color(if active { accent } else { tokens.text_primary }),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(auth) =
                                    this.pane_mut(pane_id).and_then(|p| p.chat.auth.as_mut())
                                {
                                    auth.filter_models(ModelPickerFilter::Provider(choice.clone()));
                                    cx.notify();
                                }
                            }),
                        ),
                )
            }));
        let selected_levels = selected
            .map(|model| model.thinking_levels())
            .unwrap_or_default();
        let reasoning_model = selected.filter(|model| match filter {
            ModelPickerFilter::Provider(provider) => provider == &model.provider,
            _ => true,
        });
        let levels = if reasoning_model.is_some() {
            selected_levels.as_slice()
        } else {
            &[]
        };
        let current = PaneChat::resolve_thinking_level(chat.mode_thinking_level(), levels);
        let heading = if let Some(model) = reasoning_model {
            let provider = catalog.provider_name(&model.provider);
            let title = format!("{provider} · {}", model.id);
            div()
                .id(("picker-reasoning-name", pane_id))
                .truncate()
                .child(model.id.clone())
                .debug_selector(move || format!("picker-reasoning-provider-{provider}"))
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(title.clone()).build(window, cx)
                })
                .into_any_element()
        } else {
            div().child("Select a model").into_any_element()
        };
        let reasoning = div()
            .id(("picker-reasoning", pane_id))
            .h(reasoning_height)
            .flex_shrink_0()
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(if compact { 3.0 } else { 7.0 }))
            .px(px(17.0))
            .border_t_1()
            .border_color(chip)
            .text_size(px(13.0))
            .text_color(tokens.text_secondary)
            .child(heading)
            .child(
                div()
                    .id(("picker-reasoning-levels", pane_id))
                    .h(px(if compact { 26.0 } else { 35.0 }))
                    .p(px(2.0))
                    .rounded(px(12.0))
                    .bg(chip)
                    .flex()
                    .items_center()
                    .overflow_x_scroll()
                    .when(levels.is_empty(), |bar| {
                        bar.child(
                            div()
                                .px(px(8.0))
                                .text_size(px(12.0))
                                .child("No reasoning levels available"),
                        )
                    })
                    .children(levels.iter().map(|level| {
                        let active = current == level.as_str();
                        let label = picker_effort_label(level).to_owned();
                        let level = level.clone();
                        let selector = format!("model-reasoning-{level}");
                        div()
                            .id(SharedString::from(selector.clone()))
                            .debug_selector(move || selector.clone())
                            .relative()
                            .h_full()
                            .flex_1()
                            .min_w(px(62.0))
                            .px(px(4.0))
                            .border_1()
                            .border_color(if active {
                                accent.opacity(0.54)
                            } else {
                                accent.opacity(0.0)
                            })
                            .rounded(px(9.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .cursor_pointer()
                            .when(active, |segment| {
                                segment
                                    .text_color(tokens.text_primary)
                                    .bg(gpui::linear_gradient(
                                        180.0,
                                        gpui::linear_color_stop(accent.opacity(0.36), 0.0),
                                        gpui::linear_color_stop(accent.opacity(0.16), 1.0),
                                    ))
                                    .child(
                                        svg()
                                            .path("icons/picker-effort-grid.svg")
                                            .absolute()
                                            .inset(px(3.0))
                                            .text_color(surface.opacity(0.54)),
                                    )
                            })
                            .hover(move |segment| {
                                segment.bg(if active {
                                    accent.opacity(0.32)
                                } else {
                                    row_hover
                                })
                            })
                            .child(div().min_w_0().truncate().child(label))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.set_reasoning_effort(pane_id, &level, cx);
                                }),
                            )
                    })),
            );
        let footer_chip = || {
            div()
                .h(px(32.0))
                .px(px(9.0))
                .rounded(px(10.0))
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(5.0))
                .bg(chip)
                .cursor_pointer()
        };
        let can_toggle_fast = !selected_levels.is_empty();
        let fast = can_toggle_fast && chat.fast_mode;
        let footer = div()
            .h(footer_height)
            .px(px(13.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(7.0))
            .border_t_1()
            .border_color(chip)
            .bg(surface.blend(accent.opacity(0.075)))
            .text_size(px(13.0))
            .text_color(tokens.text_secondary)
            .child(
                footer_chip()
                    .id(("picker-fast-mode", pane_id))
                    .when(fast, |button| {
                        button
                            .bg(accent.opacity(0.18))
                            .text_color(tokens.text_primary)
                    })
                    .when(!can_toggle_fast, |button| {
                        button.opacity(0.55).cursor(CursorStyle::Arrow)
                    })
                    .when(can_toggle_fast, |button| {
                        button
                            .hover(move |button| {
                                button.bg(if fast {
                                    accent.opacity(0.24)
                                } else {
                                    row_hover
                                })
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_fast_mode(pane_id, cx);
                                }),
                            )
                    })
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(if can_toggle_fast {
                            "Toggle Fast mode"
                        } else {
                            "Fast mode requires a model with reasoning levels"
                        })
                        .build(window, cx)
                    })
                    .child(
                        svg()
                            .path("icons/effort-bolt.svg")
                            .size(px(13.0))
                            .text_color(tokens.text_secondary),
                    )
                    .child("Fast"),
            )
            .child(
                footer_chip()
                    .id(("models-open-providers", pane_id))
                    .debug_selector(|| "agent-more-providers".into())
                    .hover(move |button| button.bg(row_hover))
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("Manage providers").build(window, cx)
                    })
                    .child(
                        svg()
                            .path("icons/settings.svg")
                            .size(px(13.0))
                            .text_color(tokens.text_secondary),
                    )
                    .child("Providers")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.set_agent_dialog_page(pane_id, AgentDialogPage::Auth, window, cx);
                        }),
                    ),
            )
            .child(div().flex_1().min_w_0())
            .child(self.render_agent_permission(pane, true, window, cx));
        div()
            .relative()
            .flex()
            .flex_col()
            .tab_group()
            .h((height - px(2.0)).max(px(0.0)))
            .min_h_0()
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let key = &event.keystroke;
                let shortcut = if cfg!(target_os = "macos") {
                    key.modifiers.platform
                } else {
                    key.modifiers.control
                };
                if shortcut
                    && !key.modifiers.alt
                    && !key.modifiers.shift
                    && let Ok(number) = key.key.parse::<usize>()
                    && let Some(model) =
                        number.checked_sub(1).and_then(|index| shortcuts.get(index))
                {
                    cx.stop_propagation();
                    this.select_agent_model(pane_id, Some(model.clone()), window, cx);
                }
            }))
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(surface.blend(accent.opacity(0.22)), 0.0),
                        gpui::linear_color_stop(surface.blend(accent.opacity(0.05)), 1.0),
                    ))
                    .child(
                        div()
                            .id(("model-filter", pane_id))
                            .debug_selector(|| "agent-model-filter".into())
                            .mx(px(13.0))
                            .h(search_height)
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(if filter_focused {
                                accent.opacity(0.78)
                            } else {
                                accent.opacity(0.35)
                            })
                            .child(
                                Input::new(&auth.filter)
                                    .colors(gpui_component::input::InputColors {
                                        foreground: tokens.text_primary,
                                        placeholder: tokens.text_secondary,
                                        caret: accent,
                                        selection: accent.opacity(0.25),
                                    })
                                    .w_full()
                                    .px_0()
                                    .text_size(px(15.0))
                                    .appearance(false)
                                    .bordered(false)
                                    .focus_bordered(false)
                                    .prefix(
                                        svg()
                                            .path("ui_icons/search.svg")
                                            .size(px(16.0))
                                            .text_color(tokens.text_secondary),
                                    ),
                            ),
                    )
                    .child(rail),
            )
            .child(
                div()
                    .id(("agent-models", pane_id))
                    .relative()
                    .mt(px(7.0))
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .when(!empty, |v| {
                        v.child(list)
                            .child(KitScrollbar::vertical(&auth.model_scroll).styles(|style| {
                                style
                                    .track(|track| {
                                        track
                                            .bg(surface.opacity(0.0))
                                            .border_color(surface.opacity(0.0))
                                    })
                                    .track_hover(|track| {
                                        track.bg(surface).border_color(tokens.card_border)
                                    })
                                    .track_active(|track| {
                                        track.bg(surface).border_color(tokens.card_border)
                                    })
                                    .thumb(|thumb| thumb.bg(tokens.text_primary.opacity(0.22)))
                                    .thumb_hover(|thumb| thumb.bg(accent.opacity(0.55)))
                                    .thumb_active(|thumb| thumb.bg(accent.opacity(0.75)))
                            }))
                    })
                    .when(empty, |v| {
                        v.child(
                            div()
                                .px(px(13.0))
                                .py(px(10.0))
                                .text_size(px(13.0))
                                .text_color(tokens.text_secondary)
                                .child(if loading {
                                    "Loading models…"
                                } else if !query.is_empty() {
                                    "No models match your search."
                                } else if !has_connected_providers {
                                    "No providers connected. Choose Providers to connect one."
                                } else if *filter == ModelPickerFilter::Favorites {
                                    "No favorites yet. Star a model to add it here."
                                } else {
                                    "No models available."
                                }),
                        )
                    }),
            )
            .when_some(catalog.error.as_ref(), |v, error| {
                v.child(
                    div()
                        .px(px(13.0))
                        .truncate()
                        .text_color(tokens.block_failure)
                        .child(error.clone()),
                )
            })
            .child(reasoning)
            .child(footer)
            .into_any_element()
    }
}
