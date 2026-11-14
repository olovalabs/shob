//! Remote questions use the same presentation and answer envelopes as local UI.
use std::collections::VecDeque;
use std::sync::Arc;

use crossterm::event::KeyEvent;
use rpi_tui::{Container, Editor, Focusable, Input, SelectItem, SelectList, TuiAltScreen, TUI};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::interactive_tui::{ask_user_frame, parse_ask_user_prompt, AskUserPrompt};
use crate::session_driver::DriverCommand;

pub(super) enum DialogAction {
    Answer {
        request_id: String,
        answer: Option<Value>,
    },
    Freeform(String),
}

enum DialogInput {
    Text(Arc<Input>),
    Options(Arc<SelectList>),
}

struct ActiveDialog {
    request_id: String,
    prompt: AskUserPrompt,
    input: DialogInput,
    frame: Arc<Container>,
}

impl ActiveDialog {
    fn new(
        request_id: String,
        prompt: AskUserPrompt,
        tx: mpsc::UnboundedSender<DialogAction>,
    ) -> Self {
        let (input, frame) = if prompt.kind == "input" || prompt.options.is_empty() {
            let input = Arc::new(Input::with_placeholder(
                prompt.suggest.as_deref().unwrap_or("Type your answer…"),
            ));
            let id = request_id.clone();
            let submit = tx.clone();
            input.on_submit(Arc::new(move |text| {
                let _ = submit.send(DialogAction::Answer {
                    request_id: id.clone(),
                    answer: Some(json!({
                        "values": [text], "value": text, "text": text, "kind": "input",
                    })),
                });
            }));
            let id = request_id.clone();
            input.on_escape(Arc::new(move || {
                let _ = tx.send(DialogAction::Answer {
                    request_id: id.clone(),
                    answer: None,
                });
            }));
            let frame = ask_user_frame(&prompt, input.clone(), "Enter submit · Esc cancel");
            (DialogInput::Text(input), frame)
        } else {
            let multi = prompt.allow_multiple && prompt.kind != "confirm";
            // Internal numeric values avoid collisions between option labels
            // and the freeform sentinel; the answer remains the display label.
            let mut items: Vec<SelectItem> = prompt
                .options
                .iter()
                .enumerate()
                .map(|(index, (title, description))| {
                    let item = SelectItem::new(&index.to_string(), title);
                    match description {
                        Some(text) => item.with_description(text),
                        None => item,
                    }
                })
                .collect();
            if prompt.allow_freeform && !multi && prompt.kind != "confirm" {
                items.push(SelectItem::new("freeform", "Type another answer…"));
            }
            let list = Arc::new(if multi {
                SelectList::new_multi(items, 10)
            } else {
                SelectList::new(items, 10)
            });
            let id = request_id.clone();
            let selected = tx.clone();
            list.on_select(Arc::new(move |item| {
                let action = if item.value == "freeform" {
                    DialogAction::Freeform(id.clone())
                } else {
                    DialogAction::Answer {
                        request_id: id.clone(),
                        answer: Some(json!({
                            "values": [item.display_value()], "kind": "selector",
                        })),
                    }
                };
                let _ = selected.send(action);
            }));
            if multi {
                let id = request_id.clone();
                let selected = tx.clone();
                list.on_multi_select(Arc::new(move |items| {
                    let values: Vec<&str> = items.iter().map(SelectItem::display_value).collect();
                    let _ = selected.send(DialogAction::Answer {
                        request_id: id.clone(),
                        answer: Some(json!({
                            "values": values, "kind": "selector",
                        })),
                    });
                }));
            }
            let id = request_id.clone();
            list.on_cancel(Arc::new(move || {
                let _ = tx.send(DialogAction::Answer {
                    request_id: id.clone(),
                    answer: None,
                });
            }));
            let hint = if multi {
                "Space toggle · Enter submit · Esc cancel"
            } else {
                "Enter select · Esc cancel"
            };
            let frame = ask_user_frame(&prompt, list.clone(), hint);
            (DialogInput::Options(list), frame)
        };
        Self {
            request_id,
            prompt,
            input,
            frame,
        }
    }
}

pub(super) struct ShellDialogs {
    pending: VecDeque<rpi_extensions::UiDialogRequest>,
    active: Option<ActiveDialog>,
    actions: mpsc::UnboundedSender<DialogAction>,
    dirty: bool,
}

impl ShellDialogs {
    pub(super) fn new(actions: mpsc::UnboundedSender<DialogAction>) -> Self {
        Self {
            pending: VecDeque::new(),
            active: None,
            actions,
            dirty: false,
        }
    }

    pub(super) fn has_active(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn push(&mut self, request: rpi_extensions::UiDialogRequest) {
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.request_id == request.request_id)
            || self
                .pending
                .iter()
                .any(|p| p.request_id == request.request_id)
        {
            return;
        }
        self.pending.push_back(request);
        self.advance();
    }

    fn advance(&mut self) {
        if self.active.is_none() {
            if let Some(request) = self.pending.pop_front() {
                let prompt = parse_ask_user_prompt(&request);
                self.active = Some(ActiveDialog::new(
                    request.request_id,
                    prompt,
                    self.actions.clone(),
                ));
                self.dirty = true;
            }
        }
    }

    pub(super) fn close(&mut self, request_id: &str) {
        self.pending.retain(|r| r.request_id != request_id);
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.request_id == request_id)
        {
            self.active = None;
            self.dirty = true;
            self.advance();
        }
    }

    pub(super) fn clear(&mut self) -> Vec<DriverCommand> {
        let mut ids: Vec<String> = self.pending.drain(..).map(|r| r.request_id).collect();
        if let Some(active) = self.active.take() {
            ids.push(active.request_id);
            self.dirty = true;
        }
        ids.into_iter()
            .map(|request_id| DriverCommand::UiDialogResponse {
                request_id,
                answer: None,
            })
            .collect()
    }

    pub(super) fn action(&mut self, action: DialogAction) -> Option<(String, DriverCommand)> {
        match action {
            DialogAction::Answer { request_id, answer } => {
                // Ignore callbacks queued just before a timeout closed their UI.
                if !self
                    .active
                    .as_ref()
                    .is_some_and(|a| a.request_id == request_id)
                {
                    return None;
                }
                Some((
                    request_id.clone(),
                    DriverCommand::UiDialogResponse { request_id, answer },
                ))
            }
            DialogAction::Freeform(request_id) => {
                if let Some(active) = self.active.as_ref().filter(|a| a.request_id == request_id) {
                    let mut prompt = active.prompt.clone();
                    prompt.options.clear();
                    prompt.kind = "input".to_string();
                    self.active = Some(ActiveDialog::new(request_id, prompt, self.actions.clone()));
                    self.dirty = true;
                }
                None
            }
        }
    }

    pub(super) fn handle_key(&self, key: KeyEvent) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        match &active.input {
            DialogInput::Text(input) => input.handle_key(key),
            DialogInput::Options(list) => list.handle_key(key),
        }
        true
    }

    pub(super) fn handle_paste(&self, text: &str) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        if let DialogInput::Text(input) = &active.input {
            input.handle_paste(text);
        }
        true
    }

    pub(super) fn sync_view(
        &mut self,
        slot: &Arc<Container>,
        editor: &Arc<Editor>,
        tui: &Arc<TuiAltScreen>,
    ) {
        if !self.dirty {
            return;
        }
        slot.clear();
        if let Some(active) = &self.active {
            editor.set_focused(false);
            slot.add_child(active.frame.clone());
            match &active.input {
                DialogInput::Text(input) => {
                    input.set_focused(true);
                    tui.set_focus(Some(input.clone()));
                }
                DialogInput::Options(_) => tui.set_focus(None),
            }
        } else {
            slot.add_child(editor.clone());
            editor.set_focused(true);
            tui.set_focus(Some(editor.clone()));
        }
        self.dirty = false;
        tui.request_render(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use rpi_tui::Component;

    fn request(id: &str, ui: Value) -> rpi_extensions::UiDialogRequest {
        rpi_extensions::UiDialogRequest {
            request_id: id.into(),
            tool_call_id: Some(format!("call-{id}")),
            raw: ui.clone(),
            ui,
        }
    }
    fn key(dialogs: &ShellDialogs, code: KeyCode) {
        assert!(dialogs.handle_key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn remote_selector_and_unicode_input_reply_to_their_own_request() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut dialogs = ShellDialogs::new(tx);
        dialogs.push(request(
            "first",
            json!({"question":"允许执行吗？", "options":["Yes","No"]}),
        ));
        dialogs.push(request(
            "second",
            json!({"question":"说明原因", "kind":"input"}),
        ));
        let rendered = dialogs.active.as_ref().unwrap().frame.render(60).join("\n");
        assert!(rendered.contains("允许执行吗？"));
        key(&dialogs, KeyCode::Down);
        key(&dialogs, KeyCode::Enter);
        let (id, command) = dialogs.action(rx.try_recv().unwrap()).unwrap();
        assert_eq!(id, "first");
        assert_eq!(
            command,
            DriverCommand::UiDialogResponse {
                request_id: "first".into(),
                answer: Some(json!({"values":["No"],"kind":"selector"}))
            }
        );
        dialogs.close(&id);
        assert!(dialogs.handle_paste("中文理由"));
        key(&dialogs, KeyCode::Enter);
        let (id, command) = dialogs.action(rx.try_recv().unwrap()).unwrap();
        assert_eq!(id, "second");
        assert_eq!(
            command,
            DriverCommand::UiDialogResponse {
                request_id: "second".into(),
                answer: Some(json!({
                    "values":["中文理由"],"value":"中文理由","text":"中文理由","kind":"input",
                }))
            }
        );
    }

    #[test]
    fn remote_multi_select_uses_the_local_answer_envelope() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut dialogs = ShellDialogs::new(tx);
        dialogs.push(request(
            "multi",
            json!({"options":["A","B","C"],"allowMultiple":true}),
        ));
        key(&dialogs, KeyCode::Char(' '));
        key(&dialogs, KeyCode::Down);
        key(&dialogs, KeyCode::Char(' '));
        key(&dialogs, KeyCode::Enter);
        let (_, command) = dialogs.action(rx.try_recv().unwrap()).unwrap();
        assert_eq!(
            command,
            DriverCommand::UiDialogResponse {
                request_id: "multi".into(),
                answer: Some(json!({"values":["A","B"],"kind":"selector"}))
            }
        );
    }

    #[test]
    fn remote_freeform_and_cancel_do_not_submit_an_agent_prompt() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut dialogs = ShellDialogs::new(tx);
        dialogs.push(request(
            "free",
            json!({"options":["Choice"],"allowFreeform":true}),
        ));
        key(&dialogs, KeyCode::Down);
        key(&dialogs, KeyCode::Enter);
        assert!(dialogs.action(rx.try_recv().unwrap()).is_none());
        assert!(matches!(
            dialogs.active.as_ref().unwrap().input,
            DialogInput::Text(_)
        ));
        key(&dialogs, KeyCode::Esc);
        let (_, command) = dialogs.action(rx.try_recv().unwrap()).unwrap();
        assert_eq!(
            command,
            DriverCommand::UiDialogResponse {
                request_id: "free".into(),
                answer: None
            }
        );
    }

    #[test]
    fn remote_closed_dialog_rejects_late_answer_and_advances_queue() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut dialogs = ShellDialogs::new(tx);
        dialogs.push(request(
            "expired",
            json!({"kind":"confirm","summary":"Allow?"}),
        ));
        dialogs.push(request("next", json!({"kind":"input"})));
        key(&dialogs, KeyCode::Enter);
        dialogs.close("expired");
        assert!(dialogs.action(rx.try_recv().unwrap()).is_none());
        assert_eq!(dialogs.active.as_ref().unwrap().request_id, "next");
        dialogs.push(request("queued", json!({"kind":"input"})));
        let cancellations = dialogs.clear();
        assert_eq!(cancellations.len(), 2);
        assert!(cancellations
            .iter()
            .all(|c| matches!(c, DriverCommand::UiDialogResponse { answer: None, .. })));
        assert!(!dialogs.has_active());
    }
}
