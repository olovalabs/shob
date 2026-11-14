//! Agent controls that share the space above the prompt input.

use super::*;
use crate::agent_interaction::{self, Draft, FieldDraft, Kind, Response};
use gpui_component::Disableable as _;
use gpui_component::button::ButtonVariants as _;

fn panel(tokens: &ThemeTokens) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .px(px(11.0))
        .py(px(8.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(tokens.border_subtle)
        .bg(tokens.pane_bg)
        .text_color(tokens.text_primary)
        .text_size(px(13.0))
}

impl AppState {
    pub(super) fn render_agent_request(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let request = pane
            .chat
            .requests
            .front()
            .filter(|request| request.alive())?;
        let pane_id = pane.id;
        let id = request.id;
        let draft = window.use_keyed_state(("agent-request-draft", id), cx, |window, cx| {
            let fields = request
                .fields
                .iter()
                .map(|field| FieldDraft {
                    input: (field.custom && field.kind != Kind::Toggle
                        || matches!(field.kind, Kind::Text | Kind::Number))
                    .then(|| {
                        cx.new(|cx| {
                            let placeholder = if field.options.is_empty() {
                                "Type your answer"
                            } else {
                                "Or write your own response"
                            };
                            InputState::new(window, cx).placeholder(placeholder)
                        })
                    }),
                    choices: Vec::new(),
                })
                .collect();
            Draft {
                fields,
                error: None,
            }
        });
        let mut content = div().flex().flex_col().gap(px(9.0));
        if let Some(detail) = &request.detail {
            content = content.child(
                div()
                    .text_color(tokens.text_secondary)
                    .child(detail.clone()),
            );
        }
        for (index, field) in request.fields.iter().enumerate() {
            let state = &draft.read(cx).fields[index];
            let mut row = div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(field.label.clone());
            let labels = if field.kind == Kind::Toggle {
                vec![if state.choices.is_empty() {
                    "Off".into()
                } else {
                    "On".into()
                }]
            } else {
                field.options.clone()
            };
            if !labels.is_empty() {
                let choices = div().flex().flex_col().gap(px(2.0)).children(
                    labels.into_iter().enumerate().map(|(option, label)| {
                        let draft = draft.clone();
                        let kind = field.kind;
                        let selected = state.choices.contains(&option);
                        div()
                            .id(("agent-request-choice", index * 16 + option))
                            .debug_selector(move || {
                                format!("agent-request-choice-{index}-{option}")
                            })
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(9.0))
                            .px(px(4.0))
                            .py(px(5.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .bg(if selected {
                                tokens.sidebar_item_active
                            } else {
                                gpui::transparent_black()
                            })
                            .hover(|style| style.bg(tokens.sidebar_item_hover))
                            .child(
                                div()
                                    .size(px(20.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(tokens.border)
                                    .text_color(if selected {
                                        tokens.text_primary
                                    } else {
                                        tokens.text_muted
                                    })
                                    .child(if selected {
                                        "✓".to_owned()
                                    } else {
                                        (option + 1).to_string()
                                    }),
                            )
                            .child(label)
                            .on_click(move |_, window, cx| {
                                draft.update(cx, |draft, cx| {
                                    let field = &mut draft.fields[index];
                                    if let Some(position) =
                                        field.choices.iter().position(|choice| *choice == option)
                                    {
                                        field.choices.remove(position);
                                    } else {
                                        if kind != Kind::Multiple {
                                            field.choices.clear();
                                            if let Some(input) = &field.input {
                                                input.update(cx, |input, cx| {
                                                    input.set_value("", window, cx)
                                                });
                                            }
                                        }
                                        field.choices.push(option);
                                    }
                                    draft.error = None;
                                    cx.notify();
                                });
                            })
                    }),
                );
                row = row.child(choices);
            }
            if let Some(input) = &state.input {
                row = row.child(
                    div()
                        .id(("agent-request-input", index))
                        .on_mouse_down(MouseButton::Left, {
                            let input = input.clone();
                            move |_, window, cx| {
                                input.update(cx, |input, cx| input.focus(window, cx));
                                cx.stop_propagation();
                            }
                        })
                        .debug_selector(|| "agent-request-input".into())
                        .child(
                            Input::new(input)
                                .small()
                                .appearance(false)
                                .prefix(div().text_color(tokens.text_muted).child("✎")),
                        ),
                );
            }
            content = content.child(row);
        }

        let action = |key, title, response: Response| {
            Button::new(key).small().label(title).on_click(cx.listener(
                move |this, _, window, cx| {
                    this.answer_agent_request(pane_id, id, response.clone(), window, cx);
                },
            ))
        };
        let mut actions = div().flex().flex_wrap().justify_end().gap(px(5.0)).child(
            action(
                "agent-request-dismiss",
                if request.approval { "Deny" } else { "Skip" },
                Response::Deny,
            )
            .ghost(),
        );
        let submit = |draft: Entity<Draft>| {
            cx.listener(move |this, _, window, cx| {
                this.submit_agent_request(pane_id, id, &draft, window, cx);
            })
        };
        if request.approval {
            if request.allow_session {
                actions = actions.child(
                    action(
                        "agent-request-session",
                        "Allow exact action for this session",
                        Response::AllowSession,
                    )
                    .outline(),
                );
            }
            actions = actions
                .child(action("agent-request-allow", "Allow once", Response::AllowOnce).primary());
        } else {
            actions = actions.child(
                div()
                    .id("agent-request-submit-container")
                    .debug_selector(|| "agent-request-submit".into())
                    .child(
                        Button::new("agent-request-submit")
                            .small()
                            .primary()
                            .label("Send")
                            .on_click(submit(draft.clone())),
                    ),
            );
        }

        let panel_focus = window
            .use_keyed_state(("agent-request-focus", id), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        if self.focused_pane_id == Some(pane_id) {
            let focus_input =
                draft
                    .read(cx)
                    .fields
                    .iter()
                    .zip(&request.fields)
                    .find_map(|(state, field)| {
                        field
                            .options
                            .is_empty()
                            .then(|| state.input.clone())
                            .flatten()
                    });
            let focused = window.use_keyed_state(("agent-request-focused", id), cx, |_, _| false);
            if !*focused.read(cx) {
                focused.update(cx, |focused, _| *focused = true);
                let weak = cx.weak_entity();
                let panel_focus = panel_focus.clone();
                window.defer(cx, move |window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if this
                            .pane_ref(pane_id)
                            .and_then(|pane| pane.chat.requests.front())
                            .is_some_and(|request| request.id == id)
                        {
                            if let Some(input) = focus_input {
                                input.update(cx, |input, cx| input.focus(window, cx));
                            } else {
                                panel_focus.focus(window, cx);
                            }
                        }
                    });
                });
            }
        }

        let key_draft = draft.clone();
        let approval = request.approval;
        Some(
            div().pb(px(8.0)).child(
                panel(tokens)
                    .px(px(16.0))
                    .py(px(14.0))
                    .gap(px(10.0))
                    .rounded(px(16.0))
                    .bg(tokens.card_bg)
                    .id(("agent-request", id))
                    .debug_selector(|| "agent-request-panel".into())
                    .track_focus(&panel_focus)
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            this.answer_agent_request(pane_id, id, Response::Deny, window, cx);
                        }
                    }))
                    .on_action(cx.listener(
                        move |this, _: &gpui_component::input::Escape, window, cx| {
                            cx.stop_propagation();
                            this.answer_agent_request(pane_id, id, Response::Deny, window, cx);
                        },
                    ))
                    .on_action(cx.listener(
                        move |this, _: &gpui_component::input::Enter, window, cx| {
                            if !approval {
                                cx.stop_propagation();
                                this.submit_agent_request(pane_id, id, &key_draft, window, cx);
                            }
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.0))
                                    .text_color(tokens.text_secondary)
                                    .child(
                                        div()
                                            .size(px(14.0))
                                            .rounded_full()
                                            .border_1()
                                            .border_color(tokens.text_muted)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(10.0))
                                            .child("?"),
                                    )
                                    .child(if approval { "Approval" } else { "Question" }),
                            )
                            .child(
                                Button::new("agent-request-close")
                                    .small()
                                    .ghost()
                                    .label("×")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.answer_agent_request(
                                            pane_id,
                                            id,
                                            Response::Deny,
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(request.title.clone()),
                    )
                    .child(
                        div()
                            .id(("agent-request-scroll", id))
                            .max_h(px(220.0))
                            .overflow_y_scroll()
                            .child(content),
                    )
                    .children(draft.read(cx).error.as_ref().map(|error| {
                        div()
                            .id("agent-request-error")
                            .debug_selector(|| "agent-request-error".into())
                            .text_color(tokens.block_failure)
                            .child(error.clone())
                    }))
                    .child(
                        div()
                            .id("agent-request-actions")
                            .debug_selector(|| "agent-request-actions".into())
                            .child(actions),
                    ),
            ),
        )
    }

    fn submit_agent_request(
        &mut self,
        pane_id: usize,
        id: u64,
        draft: &Entity<Draft>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self
            .pane_ref(pane_id)
            .and_then(|pane| pane.chat.requests.front())
            .filter(|request| !request.approval && request.id == id)
        else {
            return;
        };
        let values = request
            .fields
            .iter()
            .zip(&draft.read(cx).fields)
            .map(|(field, state)| {
                let text = state
                    .input
                    .as_ref()
                    .map(|input| input.read(cx).value())
                    .unwrap_or_default();
                let value = agent_interaction::field_value(field, &text, &state.choices)?;
                Ok(serde_json::json!({"field":field.label,"value":value}))
            })
            .collect::<Result<Vec<_>, String>>();
        match values {
            Ok(values) => self.answer_agent_request(
                pane_id,
                id,
                Response::Submit(serde_json::Value::Array(values)),
                window,
                cx,
            ),
            Err(error) => draft.update(cx, |draft, cx| {
                draft.error = Some(error);
                cx.notify();
            }),
        }
    }

    fn answer_agent_request(
        &mut self,
        pane_id: usize,
        id: u64,
        response: Response,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if !pane
            .chat
            .requests
            .front()
            .is_some_and(|request| request.id == id)
        {
            return;
        }
        let request = pane.chat.requests.pop_front().unwrap();
        let _ = request.reply.try_send(response);
        if pane.chat.requests.is_empty() {
            if pane.chat.running {
                pane.chat.status = SessionStatus::Working;
            }
            if let Some(input) = &pane.input_state {
                input.update(cx, |input, cx| input.focus(window, cx));
            }
        }
        cx.notify();
    }

    pub(super) fn render_agent_follow_ups(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if pane.chat.follow_ups.is_empty() {
            return None;
        }
        let id = pane.id;
        let working = pane.chat.running;
        let rows = pane
            .chat
            .follow_ups
            .iter()
            .enumerate()
            .map(|(index, prompt)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(div().flex_1().min_w_0().truncate().child(prompt.clone()))
                    .child(
                        Button::new(("agent-queue-edit", index))
                            .small()
                            .ghost()
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let Some(pane) = this.pane_mut(id) else {
                                    return;
                                };
                                let Some(input) = pane.input_state.as_ref() else {
                                    return;
                                };
                                if !input.read(cx).value().trim().is_empty() {
                                    return;
                                }
                                if let Some(text) = pane.chat.follow_ups.remove(index) {
                                    input.update(cx, |input, cx| {
                                        input.set_value(text, window, cx);
                                        input.focus(window, cx);
                                    });
                                    cx.notify();
                                }
                            }))
                            .disabled(
                                pane.input_state
                                    .as_ref()
                                    .is_none_or(|input| !input.read(cx).value().trim().is_empty()),
                            ),
                    )
                    .child(
                        Button::new(("agent-queue-send", index))
                            .small()
                            .ghost()
                            .label(if working { "Send now" } else { "Send" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_follow_up_now(id, index, cx)
                            })),
                    )
                    .child(
                        Button::new(("agent-queue-remove", index))
                            .small()
                            .ghost()
                            .label("Remove")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(pane) = this.pane_mut(id) {
                                    pane.chat.follow_ups.remove(index);
                                    cx.notify();
                                }
                            })),
                    )
            })
            .collect::<Vec<_>>();
        Some(
            div().pb(px(8.0)).child(
                panel(tokens)
                    .id(("agent-follow-up-panel", id))
                    .debug_selector(|| "agent-follow-up-panel".into())
                    .child(if pane.chat.follow_ups_paused {
                        "Follow-ups paused"
                    } else {
                        "Queued follow-ups"
                    })
                    .child(
                        div()
                            .id(("agent-follow-ups", id))
                            .max_h(px(110.0))
                            .overflow_y_scroll()
                            .children(rows),
                    ),
            ),
        )
    }

    pub(super) fn render_agent_activity(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let id = pane.id;
        let workers = subagent_chat::rows(&pane.chat.messages)
            .into_iter()
            .filter(|row| matches!(row.worker.status_label(), "Running" | "Starting"))
            .map(|row| {
                let index = row.index;
                let worker = row.worker.id.clone();
                Button::new(SharedString::from(format!("activity-{index}-{worker}")))
                    .small()
                    .outline()
                    .label(format!(
                        "{} · {}",
                        row.worker.display_title(),
                        row.worker.status_label()
                    ))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_worker_view(id, index, &worker, window, cx);
                    }))
            })
            .collect::<Vec<_>>();
        let shell = pane
            .chat
            .messages
            .iter()
            .enumerate()
            .rev()
            .take_while(|(_, message)| pane.chat.running && message.role != ChatRole::User)
            .find_map(|(index, message)| {
                let tool = message.tool.as_ref()?;
                (tool.state == ToolState::Running && is_shell_tool(&tool.name))
                    .then(|| (index, tool.display.view.subtitle.clone()))
            });
        if workers.is_empty() && shell.is_none() {
            return None;
        }
        Some(
            div().pb(px(8.0)).child(
                panel(tokens).id(("agent-activity-panel", id)).child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(5.0))
                        .children(workers)
                        .children(shell.map(|(index, command)| {
                            let label = if command.is_empty() {
                                "Command running".to_owned()
                            } else {
                                format!("Running · {}", clip_tool_text(&command, 60).0)
                            };
                            Button::new("agent-activity-shell")
                                .small()
                                .outline()
                                .label(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.reveal_agent_activity(id, index, cx);
                                }))
                        }))
                        .when(pane.chat.running, |row| {
                            row.child(
                                Button::new("agent-activity-stop")
                                    .small()
                                    .ghost()
                                    .label("Stop run")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.stop_agent_activity(id, cx)
                                    })),
                            )
                        }),
                ),
            ),
        )
    }

    pub(super) fn render_agent_recovery(
        &self,
        pane: &TerminalPane,
        tokens: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if pane.chat.running || !pane.chat.requests.is_empty() {
            return None;
        }
        let error = pane
            .chat
            .recovery
            .as_ref()
            .or(pane.chat.startup_error.as_ref())?;
        let short = clip_tool_text(error, 240).0;
        let id = pane.id;
        let missing_folder = !self.agent_working_directory(pane).is_dir();
        Some(
            div().pb(px(8.0)).child(
                panel(tokens).child(short).child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(5.0))
                        .child(
                            Button::new("agent-retry")
                                .small()
                                .outline()
                                .label("Retry")
                                .on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.retry_agent_turn(id, cx)
                                    }),
                                )
                                .disabled(pane.chat.retry_prompt.is_none() || missing_folder),
                        )
                        .when(missing_folder, |actions| {
                            actions.child(
                                Button::new("agent-recovery-folder")
                                    .small()
                                    .outline()
                                    .label("Choose folder")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.choose_agent_folder(id, cx);
                                    })),
                            )
                        })
                        .child(
                            Button::new("agent-recovery-model")
                                .small()
                                .ghost()
                                .label("Choose model")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_agent_auth(id, AgentDialogPage::Models, window, cx)
                                })),
                        )
                        .child(
                            Button::new("agent-recovery-connect")
                                .small()
                                .ghost()
                                .label("Providers")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_agent_auth(id, AgentDialogPage::Auth, window, cx)
                                })),
                        )
                        .child(
                            Button::new("agent-recovery-dismiss")
                                .small()
                                .ghost()
                                .label("Dismiss")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(pane) = this.pane_mut(id) {
                                        pane.chat.recovery = None;
                                        pane.chat.startup_error = None;
                                        cx.notify();
                                    }
                                })),
                        ),
                ),
            ),
        )
    }
}

impl AppState {
    fn send_follow_up_now(&mut self, pane_id: usize, index: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        if !pane.chat.running {
            if let Some(prompt) = pane.chat.follow_ups.remove(index) {
                pane.chat.follow_ups.push_front(prompt);
                self.send_queued_follow_up(pane_id, cx);
            }
            return;
        }
        let Some(prompt) = pane.chat.follow_ups.get(index) else {
            return;
        };
        let Some(bridge) = pane.chat.bridge.as_ref() else {
            return;
        };
        match bridge.steer(prompt) {
            Ok(()) => {
                let text = pane.chat.follow_ups.remove(index).unwrap();
                let text = format!("Sent to the current run: {text}");
                if let Some(log) = pane.chat.conversation.as_mut() {
                    log.append_local(ChatRole::Status, &text);
                }
                pane.chat
                    .messages
                    .push(Arc::new(ChatMessage::new(ChatRole::Status, text)));
                pane.chat.mark_active_dirty();
            }
            Err(error) => pane.chat.recovery = Some(error),
        }
        cx.notify();
    }

    fn reveal_agent_activity(&mut self, pane_id: usize, index: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        let (rows, _) = Self::build_chat_rows(pane, pane.chat.running);
        let Some((row, message)) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| chat_row_contains(row, index))
        else {
            return;
        };
        if let ChatRow::ToolGroup { start, .. } = message {
            pane.chat
                .expanded_tool_groups
                .entry(pane.chat.conversation_key.clone())
                .or_default()
                .insert(*start);
            pane.chat.remeasure_row(row);
        }
        pane.chat.autoscroll = false;
        pane.chat.list_state.scroll_to_reveal_item(row);
        cx.notify();
    }

    fn stop_agent_activity(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(pane_id) else {
            return;
        };
        pane.chat.follow_ups_paused = true;
        pane.chat.requests.clear();
        if let Some(bridge) = pane.chat.bridge.as_mut()
            && let Err(error) = bridge.cancel()
        {
            pane.chat.recovery = Some(error);
        }
        cx.notify();
    }

    fn choose_agent_folder(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await else {
                return;
            };
            let folder = folder.path().to_path_buf();
            let _ = this.update(cx, |this, cx| {
                if let Some(pane) = this.pane_mut(pane_id) {
                    pane.chat.workspace_override = Some(folder);
                    // A session resumed from another directory may retain that directory.
                    pane.chat.bridge = None;
                    pane.chat.rpi_session_id = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn retry_agent_turn(&mut self, pane_id: usize, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_ref(pane_id) else {
            return;
        };
        if pane.chat.running {
            return;
        }
        let Some(prompt) = pane.chat.retry_prompt.clone() else {
            return;
        };
        let cwd = self.agent_working_directory(pane);
        let save_history = self.settings.restore.agent_history;
        if let Some(pane) = self.pane_mut(pane_id) {
            Self::start_agent_turn(pane, prompt, cwd, save_history, cx);
        }
    }
}
