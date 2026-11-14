//! Extension commands, native extension dialogs, and ask-user bridging.

use super::*;

/// Encode a crossterm key into the raw key data consumed by the Node TUI
/// compatibility layer. Plain keys retain the usual terminal sequences;
/// modified functional keys use Kitty CSI-u so Shift/Alt/Ctrl combinations are
/// not collapsed into their unmodified equivalent (notably Shift+Enter).
pub(super) struct ExtensionCommand {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) session: crate::session::ExtensionSessionCell,
}

impl SlashCommand for ExtensionCommand {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &'static str {
        "extension command"
    }

    fn description_owned(&self) -> String {
        self.description.clone()
    }

    fn execute(&self, ctx: &CommandContext, args: &str) {
        let result = invoke_extension_command(&self.session, &self.name, args);
        handle_extension_ui_result(result, ctx, self.session.clone(), self.name.clone());
    }
}

pub(super) fn invoke_extension_command(
    session: &crate::session::ExtensionSessionCell,
    name: &str,
    args: &str,
) -> Option<serde_json::Value> {
    let command = session
        .lock()
        .ok()
        .and_then(|s| s.snapshot_arc())
        .and_then(|snap| {
            snap.commands()
                .iter()
                .find(|c| c.name.trim_start_matches('/') == name.trim_start_matches('/'))
                .cloned()
        })?;
    let input = serde_json::json!({ "args": args, "command": name });
    let input = serde_json::to_string(&input).ok()?;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out = rpi_plugin_sdk::StbString::empty();
        let rc = (command.handler)(
            rpi_plugin_sdk::StbStringRef::from_str(&input),
            &mut out as *mut rpi_plugin_sdk::StbString,
            command.user_data,
        );
        // SAFETY: the callback transfers a live output to its producer.
        let out = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(
                out,
                Some(rpi_extensions::host_free_string),
            )
        };

        if rc == 0 {
            Some(out.to_string_lossy())
        } else {
            None
        }
    }))
    .ok()
    .flatten()?;
    serde_json::from_str(&outcome).ok()
}

pub(super) fn handle_extension_ui_result(
    result: Option<serde_json::Value>,
    ctx: &CommandContext,
    session: crate::session::ExtensionSessionCell,
    command_name: String,
) {
    let Some(value) = result else {
        add_error_message(&ctx.state.notice_container(), "Extension command failed.");
        ctx.tui.request_render(false);
        return;
    };
    // A cancellation continuation may intentionally return JSON null. Native
    // pi resolves the pending promise with `undefined` and does not add a
    // visible "null" message to the transcript.
    if value.is_null() {
        ctx.tui.request_render(false);
        return;
    }
    match value.get("kind").and_then(|v| v.as_str()) {
        Some("message") | None => {
            if crate::transcript_view::update_plan_panel(&ctx.chat, &value) {
                ctx.tui.request_render(false);
                return;
            }
            let fallback = value.to_string();
            let text = value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or(&fallback)
                .to_string();
            if !text.is_empty() {
                if value.get("markdown").and_then(|v| v.as_bool()) == Some(true) {
                    if let Some(component) = extension_text_component(&value) {
                        ctx.chat.add_child(component);
                    }
                } else {
                    add_note_message(&ctx.state.notice_container(), &text);
                }
            }
            ctx.tui.request_render(false);
        }
        Some("selector") => open_extension_selector(ctx, session, command_name, value),
        Some("editor") => open_extension_editor(ctx, session, command_name, value),
        // upstream exposes `ctx.ui.input(title, placeholder)` separately
        // from the multiline editor. Render it as a focused single-line
        // dialog in the swapped input slot.
        Some("input") => open_extension_input(ctx, session, command_name, value),
        Some(other) => {
            add_error_message(
                &ctx.state.notice_container(),
                &format!("Unsupported extension UI: {other}"),
            );
            ctx.tui.request_render(false);
        }
    }
}

/// Render the title used by the upstream extension dialogs.  Keeping it in
/// the swapped editor container makes the question stay visible while the
/// extension waits for the answer, instead of adding a transient chat note.
pub(super) fn extension_dialog_title(title: &str, bold: bool) -> Arc<Text> {
    let colors = current_theme().colors;
    let text = if bold {
        tui_bold(title)
    } else {
        title.to_string()
    };
    Arc::new(Text::new(colors.accent.fg(&text), 1, 0))
}

pub(super) fn extension_dialog_hint(label: &str) -> Arc<Text> {
    Arc::new(Text::new(current_theme().colors.muted.fg(label), 1, 0))
}

/// Markdown-aware version of `extension_dialog_title` for extension dialogs
/// that may contain markdown content (tables, lists, bold, etc).
pub(super) fn extension_dialog_title_md(title: &str, bold: bool) -> Arc<dyn Component> {
    let text = if bold {
        format!("**{}**", title)
    } else {
        title.to_string()
    };
    Arc::new(Markdown::new(text, 1, 0))
}

/// Markdown-aware version of `extension_dialog_hint` for extension dialogs
/// that may contain markdown content.
pub(super) fn extension_dialog_hint_md(label: &str) -> Arc<dyn Component> {
    Arc::new(Markdown::new(label, 1, 0))
}

/// Take and run the cancellation callback for the active extension dialog.
/// Taking it before invoking the callback breaks the temporary Arc cycle: the
/// callback owns the command context so it can process a follow-up result.
pub(super) fn run_extension_cancel(state: &Arc<TuiState>) -> bool {
    let callback = state.active_extension_cancel.lock().unwrap().take();
    if let Some(callback) = callback {
        callback();
        true
    } else {
        false
    }
}

pub(super) fn open_extension_input(
    ctx: &CommandContext,
    session: crate::session::ExtensionSessionCell,
    command_name: String,
    value: serde_json::Value,
) {
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or("Input");
    let input = value
        .get("placeholder")
        .and_then(|v| v.as_str())
        .map(Input::with_placeholder)
        .unwrap_or_default();
    let input = Arc::new(input);
    if let Some(initial) = value
        .get("initialText")
        .or_else(|| value.get("text"))
        .and_then(|v| v.as_str())
    {
        input.set_value(initial);
    }
    input.set_focused(true);

    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(title, false));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(input.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint("Enter submit · Esc/Ctrl+C cancel"));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    *ctx.state.active_extension_editor.lock().unwrap() = None;
    *ctx.state.active_extension_input.lock().unwrap() = Some(input.clone());
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let original = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let session_submit = session.clone();
    let command_submit = command_name.clone();
    let ctx_submit = ctx.clone();
    input.on_submit(Arc::new(move |text| {
        let args = serde_json::json!({ "action": "input", "value": text, "text": text });
        let result = invoke_extension_command(
            &session_submit,
            &command_submit,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_extension_editor(&state, &ec, &original, &tui);
        handle_extension_ui_result(
            result,
            &ctx_submit,
            session_submit.clone(),
            command_submit.clone(),
        );
    }));

    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let original_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let session_cancel = session.clone();
    let command_cancel = command_name.clone();
    let ctx_cancel = ctx.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        let args = serde_json::json!({ "action": "cancel" });
        let result = invoke_extension_command(
            &session_cancel,
            &command_cancel,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_extension_editor(&state_cancel, &ec_cancel, &original_cancel, &tui_cancel);
        handle_extension_ui_result(
            result,
            &ctx_cancel,
            session_cancel.clone(),
            command_cancel.clone(),
        );
    }));

    ctx.tui.set_focus(Some(input));
    ctx.tui.request_render(false);
}

pub(super) fn open_extension_selector(
    ctx: &CommandContext,
    session: crate::session::ExtensionSessionCell,
    command_name: String,
    value: serde_json::Value,
) {
    let items = value
        .get("items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    // upstream's selector accepts `string[]`; the Rust ABI
                    // also permits `{value,label,description}` objects.
                    let value = if let Some(value) = item.as_str() {
                        value
                    } else {
                        item.get("value")?.as_str()?
                    };
                    let label = item.get("label").and_then(|v| v.as_str()).unwrap_or(value);
                    let mut out = SelectItem::new(value, label);
                    if let Some(desc) = item.get("description").and_then(|v| v.as_str()) {
                        out = out.with_description(desc);
                    }
                    Some(out)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if items.is_empty() {
        add_error_message(
            &ctx.state.notice_container(),
            "Extension selector has no items.",
        );
        ctx.tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or("Select");
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(title, true));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(list.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(
        "↑↓ navigate · Enter select · Esc/Ctrl+C cancel",
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let editor = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let session_select = session.clone();
    let command_select = command_name.clone();
    let ctx_select = ctx.clone();
    list.on_select(Arc::new(move |item| {
        let args = serde_json::json!({ "action": "select", "value": item.value });
        let result = invoke_extension_command(
            &session_select,
            &command_select,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_selector(&state, &ec, &editor, &tui);
        handle_extension_ui_result(
            result,
            &ctx_select,
            session_select.clone(),
            command_select.clone(),
        );
    }));
    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    list.on_cancel(Arc::new(move || {
        if !run_extension_cancel(&state_cancel) {
            close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
        }
    }));
    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let session_cancel = session.clone();
    let command_cancel = command_name.clone();
    let ctx_cancel = ctx.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        let args = serde_json::json!({ "action": "cancel" });
        let result = invoke_extension_command(
            &session_cancel,
            &command_cancel,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
        handle_extension_ui_result(
            result,
            &ctx_cancel,
            session_cancel.clone(),
            command_cancel.clone(),
        );
    }));
    open_selector_with_view(
        &ctx.state,
        &ctx.editor_container,
        &ctx.editor,
        &ctx.tui,
        SelectorView::List(list),
        frame,
        SelectorKind::Extension,
    );
}

pub(super) fn open_extension_editor(
    ctx: &CommandContext,
    session: crate::session::ExtensionSessionCell,
    command_name: String,
    value: serde_json::Value,
) {
    let initial = value
        .get("initialText")
        .or_else(|| value.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or("Editor");
    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            autocomplete_max_visible: 0,
            placeholder: value
                .get("placeholder")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            initial_text: Some(initial),
        },
        EditorStyle {
            prompt: "> ".to_string(),
            placeholder: String::new(),
        },
        Arc::new(rpi_tui::Keybindings::new()),
    ));
    editor.set_focused(true);
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(title, false));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(editor.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(
        "Enter submit · Shift+Enter newline · Esc/Ctrl+C cancel",
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    *ctx.state.active_extension_editor.lock().unwrap() = Some(editor.clone());
    *ctx.state.active_extension_input.lock().unwrap() = None;
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let original = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let session_submit = session.clone();
    let command_submit = command_name.clone();
    let ctx_submit = ctx.clone();
    editor.on_submit(Arc::new(move |text| {
        let args = serde_json::json!({ "action": "edit", "text": text });
        let result = invoke_extension_command(
            &session_submit,
            &command_submit,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_extension_editor(&state, &ec, &original, &tui);
        handle_extension_ui_result(
            result,
            &ctx_submit,
            session_submit.clone(),
            command_submit.clone(),
        );
    }));

    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let original_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let session_cancel = session.clone();
    let command_cancel = command_name.clone();
    let ctx_cancel = ctx.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        let args = serde_json::json!({ "action": "cancel" });
        let result = invoke_extension_command(
            &session_cancel,
            &command_cancel,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_extension_editor(&state_cancel, &ec_cancel, &original_cancel, &tui_cancel);
        handle_extension_ui_result(
            result,
            &ctx_cancel,
            session_cancel.clone(),
            command_cancel.clone(),
        );
    }));

    ctx.tui.set_focus(Some(editor));
    ctx.tui.request_render(false);
}

/// Submit callbacks close or replace the dialog, so release both state locks
/// before entering a widget. An `if let` scrutinee keeps its temporary guard
/// alive through the callback and deadlocks when it clears the same slot.
pub(super) fn handle_extension_dialog_key(state: &TuiState, key: KeyEvent) {
    let extension_editor = state.active_extension_editor.lock().unwrap().clone();
    let extension_input = state.active_extension_input.lock().unwrap().clone();
    if let Some(editor) = extension_editor {
        editor.handle_key(key);
    } else if let Some(input) = extension_input {
        input.handle_key(key);
    }
}

pub(super) fn close_extension_editor(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    editor_container.clear();
    editor_container.add_child(editor.clone());
    state.autocomplete_container.clear();
    *state.active_extension_editor.lock().unwrap() = None;
    *state.active_extension_input.lock().unwrap() = None;
    *state.active_extension_cancel.lock().unwrap() = None;
    editor.set_focused(true);
    tui.set_focus(Some(editor.clone()));
    tui.request_render(false);
}

/// Sentinel option value that switches a freeform-capable selector into the
/// single-line input slot (native `allowFreeform` affordance).
const ASK_USER_FREEFORM: &str = "\u{0}ask-user-freeform";

/// One in-flight `ask_user` request being shown in the TUI. Unlike
/// [`JsDialogBridge`], the plugin side is a synchronous `poll` loop that reads
/// answers back through the shared [`rpi_extensions::UiDialogMailbox`], so this
/// bridge only tracks which request id currently occupies the input slot.
#[derive(Clone)]
pub(super) struct AskUserBridge {
    pub(super) mailbox: rpi_extensions::UiDialogMailbox,
    pub(super) visible: Arc<Mutex<Option<String>>>,
}

impl AskUserBridge {
    pub(super) fn new(mailbox: rpi_extensions::UiDialogMailbox) -> Self {
        Self {
            mailbox,
            visible: Arc::new(Mutex::new(None)),
        }
    }

    /// Mark the interactive consumer as available. Idempotent.
    pub(super) fn attach(&self) {
        self.mailbox.attach();
    }

    /// Take the next pending request and mark it visible. Only the single key
    /// loop calls this while no other dialog is open, so the visible-id
    /// bookkeeping is race-free without a separate lock.
    /// Take the next request the TUI should draw.
    ///
    /// Skips plugin-reserved entries (`pidingo:` prefix): those belong to a
    /// remote UI's gate, so drawing them here would show the question in the
    /// terminal while the user who can answer it watches a remote client.
    pub(super) fn take_pending_for_tui(&self) -> Option<rpi_extensions::UiDialogRequest> {
        let mut visible = self.visible.lock().ok()?;
        if visible.is_some() {
            return None;
        }
        // An external UI owns delivery: taking here would steal the request and
        // draw it in a terminal the user is not watching. The ownership check
        // and the dequeue share one lock, so a relay claiming ownership cannot
        // interleave between them.
        let request = self
            .mailbox
            .take_pending_unless_external(|r| !self.mailbox.is_reserved(&r.request_id))?;
        *visible = Some(request.request_id.clone());
        Some(request)
    }

    pub(super) fn clear_visible(&self, id: &str) {
        if let Ok(mut visible) = self.visible.lock() {
            if visible.as_deref() == Some(id) {
                *visible = None;
            }
        }
    }

    /// Record an answer for `id` and release the input slot.
    pub(super) fn respond(&self, id: &str, answer: serde_json::Value) {
        let _ = self.mailbox.respond(id, answer);
        self.clear_visible(id);
    }

    /// Cancel `id` and release the input slot.
    pub(super) fn cancel(&self, id: &str) {
        let _ = self.mailbox.cancel(id);
        self.clear_visible(id);
    }

    /// Cancel every outstanding request (run abort). Pending prompts from the
    /// aborted turn must not surface in a later turn.
    pub(super) fn cancel_all(&self) {
        self.mailbox.cancel_all();
        if let Ok(mut visible) = self.visible.lock() {
            *visible = None;
        }
    }

    /// Detach + cancel everything (TUI shutdown) so a parked plugin `poll`
    /// observes a terminal state instead of blocking on a dead session.
    pub(super) fn shutdown(&self) {
        self.mailbox.detach();
        if let Ok(mut visible) = self.visible.lock() {
            *visible = None;
        }
    }
}

/// Parsed presentation data for one `ask_user` question. Supports both the
/// flat native question contract and the earlier `questions[0]` alias, plus a
/// `confirm` summary.
#[derive(Clone)]
pub(crate) struct AskUserPrompt {
    pub(crate) question_id: String,
    pub(crate) question: String,
    pub(crate) header: Option<String>,
    pub(crate) context: Option<String>,
    pub(crate) options: Vec<(String, Option<String>)>,
    pub(crate) allow_multiple: bool,
    pub(crate) allow_freeform: bool,
    pub(crate) suggest: Option<String>,
    pub(crate) kind: String,
}

pub(super) fn ask_user_str(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

pub(super) fn ask_user_bool(value: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_bool))
}

pub(super) fn ask_user_options(value: &serde_json::Value) -> Vec<(String, Option<String>)> {
    let mut options = Vec::new();
    let Some(items) = value.get("options").and_then(serde_json::Value::as_array) else {
        return options;
    };
    for item in items {
        if let Some(title) = item.as_str() {
            let title = title.trim();
            if !title.is_empty() {
                options.push((title.to_string(), None));
            }
            continue;
        }
        let Some(object) = item.as_object() else {
            continue;
        };
        let title = ["title", "label", "text", "value", "name"]
            .iter()
            .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let Some(title) = title else { continue };
        let description = object
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        options.push((title.to_string(), description));
    }
    options
}

pub(crate) fn parse_ask_user_prompt(request: &rpi_extensions::UiDialogRequest) -> AskUserPrompt {
    let ui = &request.ui;
    let kind = ui
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("selector")
        .to_string();
    // A request may carry a single flat question or a `questions[]` array. The
    // plugin emits one question per request, but accepting `questions[0]` keeps
    // the host compatible with hosts/plugins that batch.
    let nested = ui
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| items.first());
    let source = nested.filter(|value| value.is_object()).unwrap_or(ui);

    let question = ask_user_str(source, &["question", "summary", "title"])
        .or_else(|| ask_user_str(ui, &["question", "summary", "title"]))
        .unwrap_or_default();
    let question_id = ask_user_str(source, &["id", "questionId"])
        .or_else(|| ask_user_str(ui, &["id", "questionId"]))
        .unwrap_or_else(|| request.request_id.clone());
    let header = ask_user_str(source, &["header"]).or_else(|| ask_user_str(ui, &["header"]));
    let context = ask_user_str(source, &["context", "message"])
        .or_else(|| ask_user_str(ui, &["context", "message"]));
    let mut options = ask_user_options(source);
    if options.is_empty() {
        options = ask_user_options(ui);
    }
    if kind == "confirm" && options.is_empty() {
        options = vec![("Yes".to_string(), None), ("No".to_string(), None)];
    }
    let allow_multiple = ask_user_bool(source, &["allowMultiple", "allow_multiple", "multiple"])
        .or_else(|| ask_user_bool(ui, &["allowMultiple", "allow_multiple", "multiple"]))
        .unwrap_or(false);
    // Freeform defaults on when there is nothing to pick from, matching the
    // "no options ⇒ free input" contract.
    let allow_freeform =
        ask_user_bool(source, &["allowFreeform", "allow_freeform", "allow_custom"])
            .or_else(|| ask_user_bool(ui, &["allowFreeform", "allow_freeform", "allow_custom"]))
            .unwrap_or(options.is_empty());
    let suggest = ask_user_str(source, &["suggest", "placeholder"])
        .or_else(|| ask_user_str(ui, &["suggest", "placeholder"]));

    AskUserPrompt {
        question_id,
        question,
        header,
        context,
        options,
        allow_multiple,
        allow_freeform,
        suggest,
        kind,
    }
}

/// Build the shared header frame for an ask-user prompt.
/// Uses markdown-aware rendering for question and context content.
pub(crate) fn ask_user_frame(
    prompt: &AskUserPrompt,
    body: Arc<dyn Component>,
    hint: &str,
) -> Arc<Container> {
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    if let Some(header) = prompt.header.as_deref() {
        frame.add_child(extension_dialog_title_md(header, true));
    }
    if !prompt.question.trim().is_empty() {
        frame.add_child(extension_dialog_title_md(
            &prompt.question,
            prompt.header.is_none(),
        ));
    }
    if let Some(context) = prompt.context.as_deref() {
        frame.add_child(extension_dialog_hint_md(context));
    }
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(body);
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(hint));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame
}

/// Open one `ask_user` request in the TUI input slot.
pub(super) fn open_ask_user_dialog(
    ctx: &CommandContext,
    bridge: AskUserBridge,
    request: rpi_extensions::UiDialogRequest,
) {
    let prompt = parse_ask_user_prompt(&request);
    if prompt.kind == "input" || prompt.options.is_empty() {
        open_ask_user_input(ctx, bridge, request, prompt);
    } else {
        open_ask_user_selector(ctx, bridge, request, prompt);
    }
}

pub(super) fn open_ask_user_selector(
    ctx: &CommandContext,
    bridge: AskUserBridge,
    request: rpi_extensions::UiDialogRequest,
    prompt: AskUserPrompt,
) {
    let multi = prompt.allow_multiple && prompt.kind != "confirm";
    let mut items: Vec<SelectItem> = Vec::new();
    for (title, description) in &prompt.options {
        let item = SelectItem::new(title, title);
        let item = match description.as_deref() {
            Some(description) => item.with_description(description),
            None => item,
        };
        items.push(item);
    }
    // A freeform-capable single selector gets an extra sentinel row that opens
    // the input slot for a typed answer.
    if prompt.allow_freeform && !multi && prompt.kind != "confirm" {
        items.push(SelectItem::new(ASK_USER_FREEFORM, "Type another answer…"));
    }
    let list = if multi {
        Arc::new(SelectList::new_multi(items, 10))
    } else {
        Arc::new(SelectList::new(items, 10))
    };

    let request_id = request.request_id.clone();
    let state = ctx.state.clone();
    let editor_container = ctx.editor_container.clone();
    let editor = ctx.editor.clone();
    let tui = ctx.tui.clone();

    // Single-select: respond with the chosen title, or fall through to the
    // input slot for the freeform sentinel.
    let bridge_select = bridge.clone();
    let request_for_freeform = request.clone();
    let prompt_for_freeform = AskUserPrompt {
        question_id: prompt.question_id.clone(),
        question: prompt.question.clone(),
        header: prompt.header.clone(),
        context: prompt.context.clone(),
        options: Vec::new(),
        allow_multiple: false,
        allow_freeform: true,
        suggest: prompt.suggest.clone(),
        kind: "input".to_string(),
    };
    let state_select = state.clone();
    let ec_select = editor_container.clone();
    let editor_select = editor.clone();
    let tui_select = tui.clone();
    let ctx_select = ctx.clone();
    list.on_select(Arc::new(move |item: &SelectItem| {
        if item.value == ASK_USER_FREEFORM {
            close_selector(&state_select, &ec_select, &editor_select, &tui_select);
            open_ask_user_input(
                &ctx_select,
                bridge_select.clone(),
                request_for_freeform.clone(),
                prompt_for_freeform.clone(),
            );
            return;
        }
        bridge_select.respond(
            &request_id,
            serde_json::json!({
                "values": [item.display_value()],
                "kind": "selector",
            }),
        );
        close_selector(&state_select, &ec_select, &editor_select, &tui_select);
    }));

    if multi {
        let bridge_multi = bridge.clone();
        let request_id_multi = request.request_id.clone();
        let state_multi = state.clone();
        let ec_multi = editor_container.clone();
        let editor_multi = editor.clone();
        let tui_multi = tui.clone();
        list.on_multi_select(Arc::new(move |selected: &[SelectItem]| {
            let values: Vec<String> = selected
                .iter()
                .map(|item| item.display_value().to_string())
                .collect();
            bridge_multi.respond(
                &request_id_multi,
                serde_json::json!({"values": values, "kind": "selector"}),
            );
            close_selector(&state_multi, &ec_multi, &editor_multi, &tui_multi);
        }));
    }

    let bridge_cancel = bridge.clone();
    let request_id_cancel = request.request_id.clone();
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        bridge_cancel.cancel(&request_id_cancel);
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    let hint = if multi {
        "Space toggle · Enter submit · Esc cancel"
    } else {
        "Enter select · Esc cancel"
    };
    let frame = ask_user_frame(&prompt, list.clone(), hint);
    open_selector_with_view(
        &state,
        &editor_container,
        &editor,
        &tui,
        SelectorView::List(list),
        frame,
        SelectorKind::Extension,
    );
}

pub(super) fn open_ask_user_input(
    ctx: &CommandContext,
    bridge: AskUserBridge,
    request: rpi_extensions::UiDialogRequest,
    prompt: AskUserPrompt,
) {
    let placeholder = prompt
        .suggest
        .clone()
        .unwrap_or_else(|| "Type your answer…".to_string());
    let input = Arc::new(Input::with_placeholder(&placeholder));
    input.set_focused(true);

    let frame = ask_user_frame(&prompt, input.clone(), "Enter submit · Esc cancel");
    *ctx.state.active_extension_editor.lock().unwrap() = None;
    *ctx.state.active_extension_input.lock().unwrap() = Some(input.clone());
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);
    ctx.tui.set_focus(Some(input.clone()));
    ctx.tui.request_render(false);

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let editor = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let bridge_submit = bridge.clone();
    let request_id = request.request_id.clone();
    input.on_submit(Arc::new(move |text: &str| {
        bridge_submit.respond(
            &request_id,
            serde_json::json!({
                "values": [text],
                "value": text,
                "text": text,
                "kind": "input",
            }),
        );
        close_extension_editor(&state, &ec, &editor, &tui);
    }));

    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let bridge_cancel = bridge.clone();
    let request_id_cancel = request.request_id.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        bridge_cancel.cancel(&request_id_cancel);
        close_extension_editor(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
}
