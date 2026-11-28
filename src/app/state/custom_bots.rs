use crate::bot_avatar::{AvatarConfig, AvatarPreview, Face, Interaction, Motion, Palette, Shape};
use gpui_component::{Disableable as _, Selectable as _, button::ButtonVariants as _};

struct BotForm {
    editing: Option<String>,
    name: Entity<InputState>,
    instructions: Entity<InputState>,
    avatar: AvatarConfig,
    avatar_options_open: bool,
    preview: Entity<AvatarPreview>,
    previous_focus: Option<gpui::FocusHandle>,
    _subscriptions: [gpui::Subscription; 2],
}

impl BotForm {
    fn valid(&self, cx: &App) -> bool {
        !self.name.read(cx).value().trim().is_empty()
            && !self.instructions.read(cx).value().trim().is_empty()
    }
}

impl AppState {
    fn bot_for_pane(&self, pane_id: usize) -> Option<&custom_bots::Bot> {
        let &(project, session, _) = self.pane_index.get(&pane_id)?;
        let session = self.projects.get(project)?.sessions.get(session)?;
        self.bots.get(&session.uid)
    }

    fn session_uses_agent_layout(&self, session: &Session) -> bool {
        self.settings.agent_layout || self.bots.contains_key(&session.uid)
    }

    fn pane_uses_agent_layout(&self, pane_id: usize) -> bool {
        self.settings.agent_layout || self.bot_for_pane(pane_id).is_some()
    }

    fn bot_session(&self, id: &str) -> Option<(usize, usize)> {
        self.projects
            .iter()
            .enumerate()
            .filter(|(_, project)| project.projectless)
            .find_map(|(project, group)| {
                group
                    .sessions
                    .iter()
                    .position(|session| session.uid == id)
                    .map(|session| (project, session))
            })
    }

    fn prepare_bot_workspaces(workspace: &workspace_state::SavedWorkspace) {
        if workspace.bots.is_empty() {
            return;
        }
        let group = workspace.projects.iter().find(|project| project.projectless);
        let scratch = group.map_or_else(paths::scratch_dir, |project| project.path.clone());
        let saved: HashSet<_> = group
            .into_iter()
            .flat_map(|project| &project.sessions)
            .map(|session| session.uid.as_str())
            .collect();
        for bot in &workspace.bots {
            if !saved.contains(bot.id.as_str())
                && let Err(error) = std::fs::create_dir_all(scratch.join(&bot.id))
            {
                eprintln!("failed to restore bot workspace: {error}");
            }
        }
    }

    fn restore_missing_bot_threads(&mut self, cx: &mut Context<Self>) {
        if self.bots.is_empty() {
            return;
        }
        let group = self.projectless_group();
        let saved: HashSet<_> = group
            .into_iter()
            .flat_map(|index| &self.projects[index].sessions)
            .map(|session| session.uid.as_str())
            .collect();
        let mut missing: Vec<_> = self
            .bots
            .values()
            .filter(|bot| !saved.contains(bot.id.as_str()))
            .map(|bot| (bot.created_at, bot.id.clone(), bot.name.clone()))
            .collect();
        missing.sort_unstable_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let mut group = group;
        for (_, id, name) in missing {
            let dir = self.projectless_root(group).join(&id);
            if let Some(index) = self.append_bot_thread(group, id, name, dir, cx) {
                group = Some(index);
            }
        }
    }

    fn append_bot_thread(
        &mut self,
        group: Option<usize>,
        id: String,
        name: SharedString,
        dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let mut session = self.build_project_session(group, id, Some(dir), name, cx)?;
        session.panes[0].one_mode = true;
        session.panes[0].chat.active = true;
        Some(self.append_project_session(group, session))
    }

    fn create_bot_thread(
        &mut self,
        bot: custom_bots::Bot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dir = self.projectless_root(self.projectless_group()).join(&bot.id);
        let path = dir.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { std::fs::create_dir_all(path) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let result = result
                    .map_err(|error| error.to_string())
                    .and_then(|()| {
                        this.append_bot_thread(
                            this.projectless_group(),
                            bot.id.clone(),
                            bot.name.clone(),
                            dir,
                            cx,
                        )
                        .ok_or_else(|| "Could not initialize the chat pane.".to_string())
                    });
                match result {
                    Ok(project) => {
                        this.bots.insert(bot.id.clone(), bot);
                        this.projects[project].expanded = true;
                        let scope = this.sidebar_scope_indices();
                        if !scope.is_empty() && !scope.contains(&project) {
                            this.settings.sidebar.project_scope = None;
                            this.settings.sidebar.extra_project_scopes.clear();
                        }
                        this.rebuild_session_index();
                        this.sidebar_inbox_scroll.set_offset(gpui::Point::default());
                        this.schedule_workspace_save(cx);
                        cx.notify();
                    }
                    Err(error) => window.push_notification(
                        Notification::new()
                            .title("Could not create bot")
                            .message(error)
                            .with_type(NotificationType::Error),
                        cx,
                    ),
                }
            });
        })
        .detach();
    }

    fn open_bot_form(&mut self, id: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let bot = id.and_then(|id| self.bots.get(id));
        if !self.startup_loaded || (id.is_some() && bot.is_none()) {
            return;
        }
        let previous_focus = window.focused(cx);
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Bot name")
                .default_value(bot.map(|bot| bot.name.clone()).unwrap_or_default())
        });
        let instructions = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(3)
                .placeholder("What should this bot help with, and how should it respond?")
                .default_value(bot.map_or("", |bot| &*bot.system_prompt))
        });
        let avatar = bot.map_or_else(AvatarConfig::default, |bot| bot.avatar);
        let preview = cx.new(|_| AvatarPreview::new(avatar));
        self.bot_form = Some(BotForm {
            editing: id.map(str::to_owned),
            name: name.clone(),
            _subscriptions: [&name, &instructions].map(|field| {
                cx.subscribe(field, |_, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                })
            }),
            instructions,
            avatar,
            avatar_options_open: false,
            preview,
            previous_focus,
        });
        name.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn close_bot_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(form) = self.bot_form.take() {
            if let Some(focus) = form.previous_focus {
                window.focus(&focus, cx);
            }
            cx.notify();
        }
    }

    fn save_bot_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.bot_form.as_ref().filter(|form| form.valid(cx)) else {
            return;
        };
        let name = SharedString::from(form.name.read(cx).value().trim());
        let system_prompt = Arc::from(form.instructions.read(cx).value().trim());
        if let Some(id) = form.editing.as_ref() {
            if let Some(bot) = self.bots.get_mut(id) {
                bot.name = name;
                bot.system_prompt = system_prompt;
                bot.avatar = form.avatar;
            }
        } else {
            let bot = custom_bots::Bot {
                id: paths::uid("bot"),
                name,
                avatar: form.avatar,
                system_prompt,
                created_at: sidebar_now_ms(),
            };
            self.close_bot_form(window, cx);
            self.create_bot_thread(bot, window, cx);
            return;
        }
        self.close_bot_form(window, cx);
        self.schedule_workspace_save(cx);
    }

    fn confirm_delete_bot(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.bots.get(&id) else {
            return;
        };
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Delete {}?", bot.name),
            Some("This permanently deletes the bot and its dedicated chat."),
            &[
                gpui::PromptButton::Cancel("Cancel".into()),
                gpui::PromptButton::Ok("Delete".into()),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(1) {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.close_bot_form(window, cx);
                    if let Some((project, session)) = this.bot_session(&id) {
                        this.close_session(project, session, window, cx);
                    } else if this.bots.remove(&id).is_some() {
                        this.schedule_workspace_save(cx);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn render_bot_form(&self, form: &BotForm, cx: &mut Context<Self>) -> Div {
        let tokens = self.tokens(cx);
        let editing = form.editing.is_some();
        let title = if editing { "Edit bot" } else { "Create bot" };
        let field = |label: &'static str, input: &Entity<InputState>, selector: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap(px(5.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(3.0))
                        .text_size(px(13.0))
                        .child(label)
                        .when(label == "Name", |label| {
                            label.child(div().text_color(tokens.block_failure).child("*"))
                        }),
                )
                .child(
                    div().debug_selector(move || selector.into()).child(
                        Input::new(input)
                            .h(px(66.0))
                            .rounded(px(8.0))
                            .bg(tokens.card_hover),
                    ),
                )
        };
        let avatar = form.avatar;
        let palettes = Palette::ALL.into_iter().enumerate().map(|(index, palette)| {
            bot_avatar_tile(
                ("bot-palette", index),
                palette.label(),
                palette == avatar.palette,
                px(28.0),
                tokens,
            )
            .debug_selector(move || format!("custom-bot-palette-{}", palette.label()))
            .child(div().size(px(18.0)).rounded_full().bg(palette.swatch()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_bot_avatar(|avatar| avatar.palette = palette, cx);
            }))
        });
        let shapes = Shape::ALL.into_iter().enumerate().map(|(index, shape)| {
            bot_avatar_tile(
                ("bot-shape", index),
                shape.label(),
                shape == avatar.shape,
                px(8.0),
                tokens,
            )
            .debug_selector(move || format!("custom-bot-shape-{}", shape.label()))
            .size(px(34.0))
            .child(bot_avatar::avatar(AvatarConfig { shape, ..avatar }, px(28.0)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_bot_avatar(|avatar| avatar.shape = shape, cx);
            }))
        });
        let save = Button::new("save-bot")
            .debug_selector(|| "custom-bot-save".into())
            .primary()
            .h(px(36.0))
            .min_w(px(132.0))
            .rounded(px(8.0))
            .label(if editing { "Save changes" } else { "Create bot" })
            .disabled(!form.valid(cx))
            .on_click(cx.listener(|this, _, window, cx| this.save_bot_form(window, cx)));
        let close = div()
            .id("close-bot-form")
            .role(gpui::Role::Button)
            .aria_label("Cancel")
            .tab_index(0)
            .size(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(move |button| button.bg(tokens.card_hover))
            .focus_visible(move |button| button.bg(tokens.card_hover))
            .child(
                svg()
                    .path("icons/x.svg")
                    .size(px(15.0))
                    .text_color(tokens.text_secondary),
            )
            .on_click(cx.listener(|this, _, window, cx| this.close_bot_form(window, cx)));
        let delete = form.editing.as_ref().map(|id| {
            let id = id.clone();
            Button::new("delete-bot")
                .ghost()
                .small()
                .compact()
                .label("Delete bot")
                .text_color(tokens.block_failure)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_delete_bot(id.clone(), window, cx);
                }))
        });
        div()
            .absolute()
            .inset_0()
            .occlude()
            .p(px(12.0))
            .flex()
            .items_center()
            .justify_center()
            .bg(tokens.window_bg.opacity(0.68))
            .child(
                div()
                    .id("custom-bot-form")
                    .role(gpui::Role::Dialog)
                    .aria_label(title)
                    .debug_selector(|| "custom-bot-form".into())
                    .w(px(466.0))
                    .max_w_full()
                    .max_h_full()
                    .flex()
                    .flex_col()
                    .bg(tokens.card_bg)
                    .border_1()
                    .border_color(tokens.card_border)
                    .rounded(px(14.0))
                    .shadow_lg()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_between()
                            .pl(px(22.0))
                            .pr(px(12.0))
                            .pt(px(8.0))
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(title)
                            .child(close),
                    )
                    .child(
                        div()
                            .id("bot-form-body")
                            .min_h_0()
                            .overflow_y_scroll()
                            .px(px(22.0))
                            .pt(px(4.0))
                            .pb(px(12.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .gap(px(5.0))
                                    .child(
                                        div()
                                            .debug_selector(|| "custom-bot-preview".into())
                                            .child(form.preview.clone().cached(
                                                StyleRefinement::default()
                                                    .size(AvatarPreview::SIZE)
                                                    .flex_shrink_0(),
                                            )),
                                    )
                                    .child(
                                        div()
                                            .id("bot-palette-options")
                                            .role(gpui::Role::Group)
                                            .aria_label("Avatar color")
                                            .flex()
                                            .flex_wrap()
                                            .justify_center()
                                            .w(px(188.0))
                                            .max_w_full()
                                            .gap(px(4.0))
                                            .children(palettes),
                                    )
                                    .child(
                                        div()
                                            .id("bot-shape-options")
                                            .role(gpui::Role::Group)
                                            .aria_label("Avatar shape")
                                            .flex()
                                            .flex_wrap()
                                            .justify_center()
                                            .gap(px(3.0))
                                            .children(shapes),
                                    )
                                    .child(self.render_bot_avatar_options(form, tokens, cx)),
                            )
                            .child(field("Name", &form.name, "custom-bot-name"))
                            .child(field(
                                "Instructions / System prompt",
                                &form.instructions,
                                "custom-bot-instructions",
                            ))
                            .child(self.render_bot_model_row(tokens, cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .justify_center()
                            .px(px(16.0))
                            .pb(px(16.0))
                            .child(save),
                    )
                    .when_some(delete, |panel, delete| {
                        panel.child(
                            div()
                                .flex()
                                .flex_none()
                                .justify_center()
                                .pb(px(8.0))
                                .child(delete),
                        )
                    }),
            )
    }

    fn render_bot_model_row(&self, tokens: ThemeTokens, cx: &Context<Self>) -> Div {
        let pane_id = self
            .active_session()
            .and_then(|session| self.one_mode_pane(session))
            .map(|pane| pane.id);
        let model = self.settings.default_agent_model.as_ref();
        let row = div()
            .flex()
            .w_full()
            .min_w_0()
            .items_center()
            .h(px(44.0))
            .gap(px(7.0))
            .px(px(12.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(tokens.card_border)
            .bg(tokens.card_hover)
            .text_size(px(13.0))
            .child(
                div()
                    .flex_1()
                    .flex_shrink_0()
                    .text_color(tokens.text_secondary)
                    .child("Model"),
            )
            .when_some(model, |row, model| {
                row.child(
                    svg()
                        .path(provider_icon_path(&model.provider))
                        .size(px(15.0))
                        .flex_none()
                        .text_color(tokens.text_primary),
                )
            })
            .child(div().min_w_0().max_w(relative(0.65)).truncate().child(
                model.map_or_else(
                    || SharedString::from("No model selected"),
                    |model| SharedString::from(model.id.as_str()),
                ),
            ));
        let Some(pane_id) = pane_id else {
            return row;
        };
        div().child(
            row.id("custom-bot-model")
                .debug_selector(|| "custom-bot-model".into())
                .role(gpui::Role::Button)
                .aria_label("Choose the workspace model")
                .tab_index(0)
                .cursor_pointer()
                .hover(move |row| row.border_color(tokens.model_picker_accent))
                .focus_visible(move |row| row.border_color(tokens.model_picker_accent))
                .tooltip(|window, cx| {
                    gpui_component::tooltip::Tooltip::new("Changes the model for the workspace")
                        .build(window, cx)
                })
                .child(
                    svg()
                        .path("icons/chevron-down.svg")
                        .size(px(12.0))
                        .flex_none()
                        .text_color(tokens.text_secondary),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_agent_auth(pane_id, AgentDialogPage::Models, window, cx);
                })),
        )
    }

    fn render_bot_avatar_options(
        &self,
        form: &BotForm,
        tokens: ThemeTokens,
        cx: &Context<Self>,
    ) -> Div {
        let avatar = form.avatar;
        div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0))
            .child(
                Button::new("bot-avatar-options")
                    .ghost()
                    .small()
                    .compact()
                    .label("Avatar options")
                    .text_color(tokens.text_secondary)
                    .child(
                        svg()
                            .path(if form.avatar_options_open {
                                "ui_icons/chevron-up.svg"
                            } else {
                                "icons/chevron-down.svg"
                            })
                            .size(px(12.0)),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(form) = &mut this.bot_form {
                            form.avatar_options_open = !form.avatar_options_open;
                            cx.notify();
                        }
                    })),
            )
            .when(form.avatar_options_open, |options| {
                options.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .p(px(10.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(tokens.card_border)
                        .child(bot_avatar_options(
                            "Expression",
                            Face::ALL.into_iter().enumerate().map(|(index, value)| {
                                self.bot_avatar_option(
                                    ("bot-face", index),
                                    value.label(),
                                    value == avatar.face,
                                    move |avatar| avatar.face = value,
                                    cx,
                                )
                            }),
                            tokens,
                        ))
                        .child(bot_avatar_options(
                            "Motion",
                            Motion::ALL.into_iter().enumerate().map(|(index, value)| {
                                self.bot_avatar_option(
                                    ("bot-motion", index),
                                    value.label(),
                                    value == avatar.motion,
                                    move |avatar| avatar.motion = value,
                                    cx,
                                )
                            }),
                            tokens,
                        ))
                        .child(bot_avatar_options(
                            "Cursor tracking",
                            Interaction::ALL.into_iter().enumerate().map(|(index, value)| {
                                self.bot_avatar_option(
                                    ("bot-interaction", index),
                                    value.label(),
                                    value == avatar.interaction,
                                    move |avatar| avatar.interaction = value,
                                    cx,
                                )
                            }),
                            tokens,
                        )),
                )
            })
    }

    fn bot_avatar_option(
        &self,
        id: (&'static str, usize),
        label: &'static str,
        selected: bool,
        apply: impl Fn(&mut AvatarConfig) + 'static,
        cx: &Context<Self>,
    ) -> Button {
        Button::new(id)
            .small()
            .label(label)
            .selected(selected)
            .on_click(cx.listener(move |this, _, _, cx| this.update_bot_avatar(&apply, cx)))
    }

    fn update_bot_avatar(&mut self, apply: impl FnOnce(&mut AvatarConfig), cx: &mut Context<Self>) {
        let Some(form) = &mut self.bot_form else {
            return;
        };
        let mut avatar = form.avatar;
        apply(&mut avatar);
        if avatar != form.avatar {
            form.avatar = avatar;
            form.preview.update(cx, |preview, cx| {
                preview.set_config(avatar);
                cx.notify();
            });
            cx.notify();
        }
    }
}

fn bot_avatar_tile(
    id: (&'static str, usize),
    label: &'static str,
    selected: bool,
    radius: Pixels,
    tokens: ThemeTokens,
) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .aria_selected(selected)
        .tab_index(0)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(radius)
        .border_1()
        .border_color(if selected {
            tokens.model_picker_accent
        } else {
            rgba(0).into()
        })
        .when(selected, |tile| tile.bg(tokens.model_picker_accent.opacity(0.1)))
        .cursor_pointer()
        .hover(move |tile| tile.bg(tokens.card_hover))
        .focus_visible(move |tile| tile.border_color(tokens.model_picker_accent))
        .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(label).build(window, cx))
}

fn bot_avatar_options(
    label: &'static str,
    options: impl IntoIterator<Item = Button>,
    tokens: ThemeTokens,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().text_xs().text_color(tokens.text_secondary).child(label))
        .child(div().flex().flex_wrap().gap(px(4.0)).children(options))
}
