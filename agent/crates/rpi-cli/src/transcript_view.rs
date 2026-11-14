//! Transport-agnostic transcript rendering.
//!
//! This is the shared render layer behind unification ("only the transport
//! differs"): both the local interactive TUI (driven by in-process
//! `AgentEvent`s) and the remote client (driven by wire `RemoteEvent`s) build
//! their transcript from the **same** `pi-tui` components — an
//! [`AssistantMessageComponent`] per assistant message, a
//! [`ToolExecutionComponent`] per tool call, a [`BashExecutionComponent`] per
//! bash run, and user bubbles. Each side maps its native event stream into the
//! provider-free [`UiEvent`] enum and feeds it to [`TranscriptView`]; the view
//! holds the live component maps and updates them in place.
//!
//! Keeping this provider-free (only `pi-tui` + `serde_json::Value`) is what lets
//! the local host and the remote client share it without dragging `rpi_agent`
//! types into the client.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use rpi_tui::{
    render_diff, AssistantBlock, AssistantMessageComponent, AssistantMessageOptions,
    BashExecutionComponent, BashTruncation, Container, Spacer, ToolExecutionComponent, ToolStatus,
    UserMessageComponent,
};
use serde_json::Value;

/// A tool call visible in an assistant message snapshot, normalized to plain
/// data so both the local (`rpi_ai::Content::ToolCall`) and the wire
/// (`{"type":"toolCall",…}`) shapes map onto it.
#[derive(Debug, Clone, PartialEq)]
pub struct UiToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// A normalized transcript event. Both the local `AgentEvent` stream and the
/// remote `RemoteEvent` stream are projected into this enum before rendering.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// A run started (no transcript effect; kept for symmetry).
    AgentStart,
    /// A run ended: finalize any still-streaming assistant panel.
    AgentEnd,
    /// A retry was scheduled — renders as a dim notice line.
    Retry {
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
        error: String,
    },
    /// An assistant message began (carries any blocks already present).
    AssistantStart { blocks: Vec<AssistantBlock> },
    /// A streaming assistant snapshot: content blocks plus the finalized tool
    /// calls visible in the snapshot.
    AssistantUpdate {
        blocks: Vec<AssistantBlock>,
        tool_calls: Vec<UiToolCall>,
    },
    /// The assistant message finished.
    AssistantEnd { blocks: Vec<AssistantBlock> },
    /// Tool lifecycle (args/result carry the stable wire `Value` shape).
    ToolStart {
        id: String,
        name: String,
        args: Value,
    },
    ToolUpdate {
        id: String,
        name: String,
        args: Value,
        result: Value,
    },
    ToolEnd {
        id: String,
        name: String,
        result: Value,
        is_error: bool,
    },
}

/// The retained transcript: the same component maps the local `TuiState` holds,
/// so a streaming assistant message and each tool/bash panel are updated in
/// place rather than rebuilt per event.
pub struct TranscriptView {
    show_images: bool,
    chat: Arc<Container>,
    panels: Arc<Container>,
    current_assistant: Arc<Mutex<Option<Arc<AssistantMessageComponent>>>>,
    tool_components: Arc<Mutex<HashMap<String, Arc<ToolExecutionComponent>>>>,
    /// Bash executions keyed by `tool_call_id` — kept separate from the generic
    /// tool map so bash output streams into a [`BashExecutionComponent`] (command
    /// header + live preview + exit/truncation status).
    bash_components: Arc<Mutex<HashMap<String, Arc<BashExecutionComponent>>>>,
    /// When true, thinking blocks collapse to a label (toggled by the host).
    hide_thinking: Arc<Mutex<bool>>,
    /// Global tool-output expansion preference (toggled by the host).
    expanded: Arc<Mutex<bool>>,
    /// Optional transformer applied to raw assistant markdown before rendering
    /// (installed by the local host from plugin `register_markdown_transformer`
    /// handlers; always `None` on the remote client).
    markdown_transformer: Arc<Mutex<Option<MarkdownTransformer>>>,
}

/// A sync `Fn(&str) -> String` applied to raw assistant markdown. Kept as a
/// trait object so this crate stays free of any plugin types.
pub type MarkdownTransformer = Arc<dyn Fn(&str) -> String + Send + Sync>;

impl TranscriptView {
    /// Self-contained view owning fresh maps (the remote client).
    pub fn new(chat: Arc<Container>) -> Self {
        Self::with_maps(
            chat,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(false)),
            Arc::new(Mutex::new(false)),
            Arc::new(Mutex::new(None)),
        )
    }

    /// Wrap **shared** component maps. The local host keeps direct `Arc` handles
    /// to these maps for its render-tick, reload and toggle logic, so it hands
    /// the same handles to the view instead of duplicating them.
    #[allow(clippy::too_many_arguments)]
    pub fn with_maps(
        chat: Arc<Container>,
        current_assistant: Arc<Mutex<Option<Arc<AssistantMessageComponent>>>>,
        tool_components: Arc<Mutex<HashMap<String, Arc<ToolExecutionComponent>>>>,
        bash_components: Arc<Mutex<HashMap<String, Arc<BashExecutionComponent>>>>,
        hide_thinking: Arc<Mutex<bool>>,
        expanded: Arc<Mutex<bool>>,
        markdown_transformer: Arc<Mutex<Option<MarkdownTransformer>>>,
    ) -> Self {
        Self {
            show_images: true,
            panels: chat.clone(),
            chat,
            current_assistant,
            tool_components,
            bash_components,
            hide_thinking,
            expanded,
            markdown_transformer,
        }
    }

    pub(crate) fn with_panels(mut self, panels: Arc<Container>) -> Self {
        self.panels = panels;
        self
    }

    /// Configure image visibility for newly created assistant components.
    pub fn with_show_images(mut self, show: bool) -> Self {
        self.show_images = show;
        self
    }

    /// Append a user prompt bubble.
    pub fn add_user(&self, text: &str) {
        self.chat
            .add_child(Arc::new(UserMessageComponent::new(text.to_string())));
        self.chat.add_child(Arc::new(Spacer::new(1)));
    }

    /// Append a dim one-line notice (retry / error / command feedback).
    pub fn add_notice(&self, text: &str) {
        self.chat
            .add_child(Arc::new(rpi_tui::Text::new(format!("· {text}"), 1, 0)));
        // One convention for every transcript entry: a trailing blank row,
        // never a leading one — without it the next block would render flush
        // against the notice.
        self.chat.add_child(Arc::new(Spacer::new(1)));
    }

    /// Fold one normalized event into the retained components. `width` is the
    /// current terminal width, used to render `edit` diffs.
    pub fn apply(&self, event: &UiEvent, width: usize) {
        match event {
            UiEvent::AgentStart => {}
            UiEvent::AgentEnd => {
                if let Some(comp) = self.current_assistant.lock().unwrap().take() {
                    comp.set_streaming(false);
                }
            }
            UiEvent::Retry {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => {
                self.add_notice(&format!(
                    "retry {attempt}/{max_retries} in {delay_ms}ms: {error}"
                ));
            }
            UiEvent::AssistantStart { blocks } => {
                self.start_assistant(blocks);
            }
            UiEvent::AssistantUpdate { blocks, tool_calls } => {
                self.ensure_assistant();
                self.sync_tool_calls(tool_calls);
                if let Some(comp) = self.current_assistant.lock().unwrap().as_ref() {
                    comp.update_blocks(blocks);
                }
            }
            UiEvent::AssistantEnd { blocks } => {
                if let Some(comp) = self.current_assistant.lock().unwrap().take() {
                    comp.update_blocks(blocks);
                    comp.set_streaming(false);
                }
            }
            UiEvent::ToolStart { id, name, args } => self.tool_start(id, name, args, width),
            UiEvent::ToolUpdate {
                id,
                name,
                args,
                result,
            } => self.tool_update(id, name, args, result, width),
            UiEvent::ToolEnd {
                id,
                name,
                result,
                is_error,
            } => self.tool_end(id, name, result, *is_error, width),
        }
    }

    /// Set the global tool-output expansion preference and re-apply it to every
    /// live panel.
    pub fn set_expanded(&self, expanded: bool) {
        *self.expanded.lock().unwrap() = expanded;
        for child in self.chat.get_children() {
            if let Some(component) = child.as_any().downcast_ref::<ToolExecutionComponent>() {
                component.set_expanded(expanded);
            }
        }
        for comp in self.tool_components.lock().unwrap().values() {
            comp.set_expanded(expanded);
        }
        for comp in self.bash_components.lock().unwrap().values() {
            comp.set_expanded(expanded);
        }
    }

    /// Flip the expansion preference; returns the new value.
    pub fn toggle_expanded(&self) -> bool {
        let next = !*self.expanded.lock().unwrap();
        self.set_expanded(next);
        next
    }

    /// Whether tool outputs are currently expanded.
    pub fn expanded(&self) -> bool {
        *self.expanded.lock().unwrap()
    }

    /// Hide/show thinking blocks on the in-flight assistant message.
    pub fn set_hide_thinking(&self, hide: bool) {
        *self.hide_thinking.lock().unwrap() = hide;
        if let Some(comp) = self.current_assistant.lock().unwrap().as_ref() {
            comp.set_hide_thinking(hide);
        }
    }

    /// Flip the thinking-visibility preference; returns the new value.
    pub fn toggle_hide_thinking(&self) -> bool {
        let next = !*self.hide_thinking.lock().unwrap();
        self.set_hide_thinking(next);
        next
    }

    /// Whether thinking blocks are currently hidden.
    pub fn hide_thinking(&self) -> bool {
        *self.hide_thinking.lock().unwrap()
    }

    /// Install (or clear) the assistant-markdown transformer, re-applying it to
    /// the in-flight assistant message immediately (mirrors the local reload).
    pub fn set_markdown_transformer(&self, transformer: Option<MarkdownTransformer>) {
        *self.markdown_transformer.lock().unwrap() = transformer.clone();
        if let Some(comp) = self.current_assistant.lock().unwrap().as_ref() {
            comp.set_markdown_transformer(transformer);
        }
    }

    /// Finalize any still-streaming assistant panel (used at turn/run end).
    pub fn finalize_assistant(&self) {
        if let Some(comp) = self.current_assistant.lock().unwrap().take() {
            comp.set_streaming(false);
        }
    }

    /// Finalize the assistant panel with an authoritative final block list.
    pub fn finalize_assistant_blocks(&self, blocks: &[AssistantBlock]) {
        if let Some(comp) = self.current_assistant.lock().unwrap().take() {
            comp.update_blocks(blocks);
            comp.set_streaming(false);
        }
    }

    /// Whether any tool panel is still running (drives the local render tick).
    pub fn has_running_tool(&self) -> bool {
        self.tool_components
            .lock()
            .unwrap()
            .values()
            .any(|component| component.status() == ToolStatus::Running)
    }

    /// Whether a bash panel is currently on screen.
    pub fn has_active_bash(&self) -> bool {
        !self.bash_components.lock().unwrap().is_empty()
    }

    fn start_assistant(&self, blocks: &[AssistantBlock]) {
        let comp = Arc::new(AssistantMessageComponent::new(
            AssistantMessageOptions::default(),
        ));
        comp.set_hide_thinking(*self.hide_thinking.lock().unwrap());
        comp.set_show_images(self.show_images);
        comp.set_trailing_spacing(1);
        if let Some(transformer) = self.markdown_transformer.lock().unwrap().clone() {
            comp.set_markdown_transformer(Some(transformer));
        }
        comp.set_streaming(true);
        comp.update_blocks(blocks);
        self.chat.add_child(comp.clone());
        *self.current_assistant.lock().unwrap() = Some(comp);
    }

    /// Create the streaming assistant component if a delta arrives before
    /// `assistant_start` (the wire does not strictly guarantee ordering).
    fn ensure_assistant(&self) {
        let mut current = self.current_assistant.lock().unwrap();
        if current.is_none() {
            let comp = Arc::new(AssistantMessageComponent::new(
                AssistantMessageOptions::default(),
            ));
            comp.set_hide_thinking(*self.hide_thinking.lock().unwrap());
            comp.set_show_images(self.show_images);
            comp.set_trailing_spacing(1);
            if let Some(transformer) = self.markdown_transformer.lock().unwrap().clone() {
                comp.set_markdown_transformer(Some(transformer));
            }
            comp.set_streaming(true);
            self.chat.add_child(comp.clone());
            *current = Some(comp);
        }
    }

    /// Pre-create panels for the tool calls visible in an assistant snapshot. A
    /// later `tool_start` coalesces onto the same entry.
    fn sync_tool_calls(&self, calls: &[UiToolCall]) {
        for call in calls {
            if matches!(
                call.name.as_str(),
                "todo" | "plan_mode_start" | "plan_mode_complete"
            ) {
                continue;
            }
            if call.name == "bash" {
                let command = call
                    .args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if command.trim().is_empty() {
                    continue;
                }
                let mut bash = self.bash_components.lock().unwrap();
                if let Some(existing) = bash.get(&call.id) {
                    existing.set_command(command);
                } else {
                    let comp = Arc::new(BashExecutionComponent::new(command));
                    comp.set_expanded(self.expanded());
                    self.chat.add_child(comp.clone());
                    bash.insert(call.id.clone(), comp);
                }
                continue;
            }
            let ask_user = is_ask_user_tool(&call.name);
            let display = if ask_user {
                ask_user_args_display(&call.args)
            } else {
                call.args.to_string()
            };
            let mut tools = self.tool_components.lock().unwrap();
            if let Some(existing) = tools.get(&call.id) {
                if let Some(skill) = skill_tool_name(&call.name, &call.args) {
                    existing.set_skill_name(skill);
                }
                if !display.trim().is_empty() && display.trim() != "{}" {
                    existing.set_args(&display);
                }
            } else {
                let comp = Arc::new(ToolExecutionComponent::new(&call.name, &display));
                if ask_user {
                    comp.set_display_title("ASK USER");
                }
                if let Some(skill) = skill_tool_name(&call.name, &call.args) {
                    comp.set_skill_name(skill);
                }
                comp.set_expanded(self.expanded());
                comp.set_running();
                self.chat.add_child(comp.clone());
                tools.insert(call.id.clone(), comp);
            }
        }
    }

    fn tool_start(&self, id: &str, name: &str, args: &Value, _width: usize) {
        if matches!(name, "todo" | "plan_mode_start" | "plan_mode_complete") {
            return;
        }
        if name.trim().is_empty() {
            return;
        }
        if name == "bash" {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            // Arguments can still be `{}` when the lifecycle event races streamed
            // arg finalization — defer until the command lands.
            if command.trim().is_empty() {
                return;
            }
            let mut bash = self.bash_components.lock().unwrap();
            if let Some(existing) = bash.get(id) {
                existing.set_command(command);
            } else {
                let comp = Arc::new(BashExecutionComponent::new(command));
                comp.set_expanded(self.expanded());
                self.chat.add_child(comp.clone());
                bash.insert(id.to_string(), comp);
            }
            return;
        }
        let ask_user = is_ask_user_tool(name);
        let display = if ask_user {
            ask_user_args_display(args)
        } else {
            args.to_string()
        };
        let mut tools = self.tool_components.lock().unwrap();
        if let Some(existing) = tools.get(id) {
            if ask_user {
                existing.set_display_title("ASK USER");
            }
            if let Some(skill) = skill_tool_name(name, args) {
                existing.set_skill_name(skill);
            }
            existing.set_args(&display);
        } else {
            let comp = Arc::new(ToolExecutionComponent::new(name, &display));
            if ask_user {
                comp.set_display_title("ASK USER");
            }
            if let Some(skill) = skill_tool_name(name, args) {
                comp.set_skill_name(skill);
            }
            comp.set_expanded(self.expanded());
            comp.set_running();
            self.chat.add_child(comp.clone());
            tools.insert(id.to_string(), comp);
        }
    }

    fn tool_update(&self, id: &str, name: &str, args: &Value, result: &Value, width: usize) {
        if matches!(name, "todo" | "plan_mode_start" | "plan_mode_complete") {
            return;
        }
        if name.trim().is_empty() {
            return;
        }
        let ask_user = is_ask_user_tool(name);
        let text = if ask_user {
            ask_user_progress_text(args, result)
        } else {
            tool_result_text(result)
        };
        if name == "bash" {
            if let Some(bash) = self.bash_components.lock().unwrap().get(id) {
                if let Some(command) = args.get("command").and_then(Value::as_str) {
                    bash.set_command(command);
                }
                if !text.is_empty() {
                    bash.append_output(&text);
                }
            }
            return;
        }
        let existing = self.tool_components.lock().unwrap().get(id).cloned();
        match existing {
            Some(comp) => {
                if let Some(skill) = skill_tool_name(name, args) {
                    comp.set_skill_name(skill);
                }
                if ask_user {
                    comp.set_display_title("ASK USER");
                    comp.set_args(&ask_user_args_display(args));
                } else if !args.is_null() && args != &serde_json::json!({}) {
                    comp.set_args(&args.to_string());
                }
                if tool_update_has_payload(&text, result) {
                    comp.set_result(&text, false);
                    if !ask_user {
                        apply_edit_diff(&comp, name, result.get("details"), width);
                    }
                }
            }
            None => {
                // Update before start (fast tool / out-of-order): create the
                // running panel now so the partial has somewhere to land.
                if tool_update_has_payload(&text, result) {
                    let display = if ask_user {
                        ask_user_args_display(args)
                    } else {
                        args.to_string()
                    };
                    let comp = Arc::new(ToolExecutionComponent::new(name, &display));
                    if ask_user {
                        comp.set_display_title("ASK USER");
                    }
                    if let Some(skill) = skill_tool_name(name, args) {
                        comp.set_skill_name(skill);
                    }
                    comp.set_expanded(self.expanded());
                    comp.set_running();
                    comp.set_result(&text, false);
                    if !ask_user {
                        apply_edit_diff(&comp, name, result.get("details"), width);
                    }
                    self.chat.add_child(comp.clone());
                    self.tool_components
                        .lock()
                        .unwrap()
                        .insert(id.to_string(), comp);
                }
            }
        }
    }

    fn tool_end(&self, id: &str, name: &str, result: &Value, is_error: bool, width: usize) {
        if matches!(name, "plan_mode_start" | "plan_mode_complete")
            && !is_error
            && update_plan_panel(&self.panels, result)
        {
            return;
        }
        if name == "todo" && !is_error && update_todo_list(&self.panels, result) {
            return;
        }
        if name.trim().is_empty() {
            return;
        }
        if name == "bash" {
            let bash = self.bash_components.lock().unwrap().remove(id);
            match bash {
                Some(comp) => finalize_bash(&comp, result, is_error),
                None => {
                    let command = result
                        .get("details")
                        .and_then(|details| details.get("command"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let text = tool_result_text(result);
                    if command.trim().is_empty() && text.trim().is_empty() {
                        return;
                    }
                    let comp = Arc::new(BashExecutionComponent::new(command));
                    comp.set_expanded(self.expanded());
                    if !text.is_empty() {
                        comp.append_output(&text);
                    }
                    finalize_bash(&comp, result, is_error);
                    self.chat.add_child(comp);
                }
            }
            return;
        }
        let ask_user = is_ask_user_tool(name);
        let text = if ask_user {
            ask_user_result_text(result)
        } else {
            tool_result_text(result)
        };
        let details = result.get("details");
        let existing = self.tool_components.lock().unwrap().remove(id);
        let comp = match existing {
            Some(comp) => comp,
            None => {
                // Tool ended without a start/update — render a finalized panel.
                let comp = Arc::new(ToolExecutionComponent::new(name, ""));
                if ask_user {
                    comp.set_display_title("ASK USER");
                }
                comp.set_expanded(self.expanded());
                self.chat.add_child(comp.clone());
                comp
            }
        };
        comp.set_result(&text, is_error);
        if !ask_user {
            if tool_result_requests_markdown(details) {
                comp.set_result_markdown(true);
            }
            apply_edit_diff(&comp, name, details, width);
        }
    }
}

/// Tool calls, slash commands, and restored entries share the same current plan.
pub(crate) fn update_plan_panel(chat: &Arc<Container>, result: &Value) -> bool {
    let Some(details) = result.get("details") else {
        return false;
    };
    if details.get("kind").and_then(Value::as_str) != Some("plan") || details.get("error").is_some()
    {
        return false;
    }
    let Some(active) = details.get("active").and_then(Value::as_bool) else {
        return false;
    };
    let plan = match details.get("plan") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(plan)) => plan.clone(),
        _ => return false,
    };
    for child in chat.get_children().into_iter().rev() {
        if let Some(component) = child.as_any().downcast_ref::<ToolExecutionComponent>() {
            if component.is_plan_panel() {
                component.set_plan_state(plan, active);
                component.set_result("", false);
                if let Some(expanded) = details.get("expanded").and_then(Value::as_bool) {
                    component.set_expanded(expanded);
                }
                // Keep the panel at the position where planning started. Moving
                // it to the tail makes all later transcript rows jump above the
                // completed plan and, in regular mode, turns the reorder into a
                // native-scrollback rebuild on macOS.
                return true;
            }
        }
    }
    let component = Arc::new(ToolExecutionComponent::new("plan_mode_complete", ""));
    component.set_plan_state(plan, active);
    component.set_result("", false);
    component.set_expanded(
        details
            .get("expanded")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    chat.add_child(component);
    true
}

/// Keep one visible current task list while retaining every result in session storage.
pub(crate) fn update_todo_list(chat: &Arc<Container>, result: &Value) -> bool {
    let Some(details) = result.get("details") else {
        return false;
    };
    if details.get("error").is_some() {
        return false;
    }
    let Some(items) = details.get("todos").and_then(Value::as_array) else {
        return false;
    };
    let rows: Option<Vec<_>> = items
        .iter()
        .map(|item| {
            let text = item.get("text")?.as_str()?.to_owned();
            let status = if item.get("done").and_then(Value::as_bool) == Some(true) {
                "completed"
            } else {
                item.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
            };
            Some((text, status.to_owned()))
        })
        .collect();
    let Some(rows) = rows else {
        return false;
    };
    let text = tool_result_text(result);
    for child in chat.get_children().into_iter().rev() {
        if let Some(component) = child.as_any().downcast_ref::<ToolExecutionComponent>() {
            if component.is_todo_list() {
                component.set_todo_state(rows);
                component.set_result(&text, false);
                chat.remove_child(&child);
                chat.add_child(child);
                return true;
            }
        }
    }
    let candidate = Arc::new(ToolExecutionComponent::new("todo", ""));
    candidate.set_todo_state(rows);
    candidate.set_result(&text, false);
    chat.add_child(candidate);
    true
}

/// Map a wire/local assistant message's content blocks into the provider-free
/// [`AssistantBlock`] list the shared component renders (text + thinking +
/// decoded image blocks, in document order).
pub fn assistant_blocks_from_value(message: &Value) -> Vec<AssistantBlock> {
    let mut blocks = Vec::new();
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return blocks;
    };
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    blocks.push(AssistantBlock::Text(text.to_string()));
                }
            }
            Some("thinking") => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    blocks.push(AssistantBlock::Thinking(text.to_string()));
                }
            }
            Some("image") => {
                if let Some(data) = block.get("data").and_then(Value::as_str) {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        if !bytes.is_empty() {
                            let png = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                                Some(bytes)
                            } else {
                                rpi_tools::image_processing::process_image(
                                    &bytes,
                                    Default::default(),
                                )
                                .ok()
                            };
                            if let Some(png) = png {
                                blocks.push(AssistantBlock::Image(png));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    blocks
}

/// Extract the finalized tool calls (`type: "toolCall"`) from an assistant
/// message value, in document order.
pub fn tool_calls_from_value(message: &Value) -> Vec<UiToolCall> {
    let mut calls = Vec::new();
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return calls;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
            continue;
        }
        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() || name.trim().is_empty() {
            continue;
        }
        calls.push(UiToolCall {
            id: id.to_string(),
            name: name.to_string(),
            args: block.get("arguments").cloned().unwrap_or(Value::Null),
        });
    }
    calls
}

/// Text out of a wire tool-result payload (`{content:[{type:"text",text}]}`).
pub fn tool_result_text(result: &Value) -> String {
    let mut out = String::new();
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
    }
    out
}

/// Whether a tool result opts into markdown rendering of its body
/// (`details.markdown = true`).
pub fn tool_result_requests_markdown(details: Option<&Value>) -> bool {
    details
        .and_then(|details| details.get("markdown"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The skill name when `tool_name` is a `read` of a `SKILL.md` file (the
/// `SKILL.md` parent directory's basename), else `None`. A skill read renders
/// as upstream's `[skill] <name>` box.
pub fn skill_tool_name(tool_name: &str, args: &Value) -> Option<String> {
    if tool_name != "read" {
        return None;
    }
    let path = args.get("path").and_then(Value::as_str)?;
    let normalized = path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next()?;
    if !file_name.eq_ignore_ascii_case("SKILL.md") {
        return None;
    }
    normalized
        .trim_end_matches('/')
        .rsplit('/')
        .nth(1)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Tool names rendered through the dedicated `ASK USER` panel instead of a
/// generic tool box. Native `ask_user` plus the JS `ask_user_question` alias.
fn is_ask_user_tool(tool_name: &str) -> bool {
    matches!(tool_name, "ask_user" | "ask_user_question")
}

/// Render ask_user tool-call ARGUMENTS as a readable question. The raw
/// transport JSON must never leak into the transcript.
fn ask_user_args_display(args: &Value) -> String {
    if args.is_null() {
        return String::new();
    }
    if let Some(summary) = args
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return format!("Confirmation requested: {summary}");
    }

    let mut lines: Vec<String> = Vec::new();
    let questions: Vec<&Value> = args
        .get("questions")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .map(|items| items.iter().collect())
        .unwrap_or_else(|| vec![args]);

    for question in questions {
        let prompt = question
            .get("question")
            .and_then(Value::as_str)
            .or_else(|| args.get("question").and_then(Value::as_str))
            .map(str::trim)
            .unwrap_or_default();
        if prompt.is_empty() {
            continue;
        }
        if let Some(header) = question
            .get("header")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            lines.push(format!("{header}: {prompt}"));
        } else {
            lines.push(prompt.to_string());
        }
        let context = question
            .get("context")
            .and_then(Value::as_str)
            .or_else(|| args.get("context").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty());
        if let Some(context) = context {
            lines.push(format!("  {context}"));
        }
        let options = question
            .get("options")
            .and_then(Value::as_array)
            .or_else(|| args.get("options").and_then(Value::as_array));
        if let Some(options) = options {
            let labels: Vec<String> = options
                .iter()
                .filter_map(|option| {
                    option
                        .get("title")
                        .and_then(Value::as_str)
                        .or_else(|| option.as_str())
                        .map(str::trim)
                        .filter(|label| !label.is_empty())
                        .map(str::to_string)
                })
                .collect();
            if !labels.is_empty() {
                lines.push(format!("Choices: {}", labels.join(", ")));
            }
        }
        if let Some(suggest) = question
            .get("suggest")
            .and_then(Value::as_str)
            .or_else(|| args.get("suggest").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            lines.push(format!("Suggestions: {suggest}"));
        }
    }

    lines.join("\n")
}

/// Progress text while an ask_user tool is pending. Prefer the tool's own
/// readable `content`, otherwise show the question so the panel is not blank.
fn ask_user_progress_text(args: &Value, result: &Value) -> String {
    let text = tool_result_text(result);
    if !text.trim().is_empty() {
        return text;
    }
    let args_text = ask_user_args_display(args);
    if args_text.trim().is_empty() {
        "Waiting for your answer…".to_string()
    } else {
        args_text
    }
}

/// Final text for a completed ask_user tool.
fn ask_user_result_text(result: &Value) -> String {
    let text = tool_result_text(result);
    if text.trim().is_empty() {
        "Answer recorded.".to_string()
    } else {
        text
    }
}

/// Whether a partial tool result carries anything worth rendering.
fn tool_update_has_payload(text: &str, result: &Value) -> bool {
    !text.trim().is_empty()
        || result
            .get("details")
            .map(|details| !details.is_null())
            .unwrap_or(false)
}

/// Attach a colored diff to an `edit` tool panel. `write` carries no diff and
/// stays a plain summary.
fn apply_edit_diff(
    comp: &Arc<ToolExecutionComponent>,
    tool_name: &str,
    details: Option<&Value>,
    width: usize,
) {
    if tool_name != "edit" {
        return;
    }
    let Some(diff_text) = details
        .and_then(|details| details.get("diff"))
        .and_then(Value::as_str)
    else {
        return;
    };
    if diff_text.is_empty() {
        return;
    }
    comp.set_diff(render_diff(diff_text, width));
}

/// Mark a bash panel complete from the wire result payload. The exit code is
/// derived from `is_error`; truncation info is read from `details`.
fn finalize_bash(comp: &Arc<BashExecutionComponent>, result: &Value, is_error: bool) {
    let details = result.get("details");
    let exit_code = Some(if is_error { 1 } else { 0 });
    let truncated = details
        .and_then(|details| details.get("truncation"))
        .and_then(|truncation| truncation.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let full_output_path = details
        .and_then(|details| details.get("full_output_path"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(command) = details
        .and_then(|details| details.get("command"))
        .and_then(Value::as_str)
    {
        comp.set_command(command);
    }
    comp.set_complete(
        exit_code,
        false,
        BashTruncation {
            truncated,
            full_output_path,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::{strip_ansi, Component};
    use serde_json::json;

    fn view() -> TranscriptView {
        TranscriptView::new(Arc::new(Container::new()))
    }

    #[test]
    fn adjacent_transcript_blocks_separated_by_exactly_one_blank() {
        // The seam invariant: any two adjacent transcript blocks — whatever
        // their kinds — are separated by exactly one blank line. Every entry
        // contributes one trailing blank (assistant trailing spacing, panel
        // trailing spacer, host trailing Spacer for user/notice entries), and
        // no block opens with a leading spacer, so the gap never depends on
        // which kind sits on top. Old behavior: a panel above an assistant
        // message gave 0 blanks (thinking glued to the code card) while an
        // assistant message above a panel gave 2 (trailing spacing + panel
        // top spacer stacking).
        //
        // Only *truly* empty rows count as a gap: a panel's own padding rows
        // carry a background color (non-empty ANSI payload) and belong to the
        // panel, not to the seam.
        let view = view();
        view.add_user("question");
        view.apply(
            &UiEvent::AssistantStart {
                blocks: vec![AssistantBlock::Thinking("reasoning".into())],
            },
            80,
        );
        view.apply(
            &UiEvent::AssistantUpdate {
                blocks: vec![AssistantBlock::Thinking("reasoning".into())],
                tool_calls: vec![UiToolCall {
                    id: "t1".into(),
                    name: "read".into(),
                    args: json!({"path": "f.txt"}),
                }],
            },
            80,
        );
        view.apply(
            &UiEvent::ToolEnd {
                id: "t1".into(),
                name: "read".into(),
                result: json!({"content": [{"type": "text", "text": "done"}]}),
                is_error: false,
            },
            80,
        );
        view.apply(
            &UiEvent::AssistantStart {
                blocks: vec![AssistantBlock::Text("answer".into())],
            },
            80,
        );
        view.apply(
            &UiEvent::ToolStart {
                id: "b1".into(),
                name: "bash".into(),
                args: json!({"command": "ls"}),
            },
            80,
        );
        view.apply(&UiEvent::AgentEnd, 80);

        let lines = view.chat.render(80);
        let plain: Vec<String> = lines
            .iter()
            .map(|l| strip_ansi(l).trim().to_string())
            .collect();
        // Anchor each block by its LAST content line, so the rows between two
        // anchors are exactly the seam between the two blocks.
        let find = |needle: &str| {
            plain
                .iter()
                .rposition(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing: {plain:?}"))
        };
        let user = find("question");
        let thinking = find("reasoning");
        let panel = find("done");
        let answer = find("answer");
        let bash = find("$ ls");
        assert!(user < thinking && thinking < panel && panel < answer);
        assert!(answer < bash);
        for (top_name, top, bottom_name, bottom) in [
            ("user", user, "thinking", thinking),
            ("thinking", thinking, "panel", panel),
            ("panel", panel, "answer", answer),
            ("answer", answer, "bash", bash),
        ] {
            let gap = lines[top + 1..bottom]
                .iter()
                .filter(|l| l.is_empty())
                .count();
            assert_eq!(
                gap, 1,
                "{top_name}→{bottom_name} seam must be exactly one blank row: {plain:?}"
            );
        }
    }

    #[test]
    fn retry_notice_ends_with_a_trailing_spacer() {
        // A notice is a transcript entry like any other: without its own
        // trailing blank it would render flush against the next block.
        let view = view();
        view.apply(
            &UiEvent::Retry {
                attempt: 1,
                max_retries: 3,
                delay_ms: 500,
                error: "boom".into(),
            },
            80,
        );
        let lines = view.chat.render(80);
        let rendered = strip_ansi(&lines.join("\n"));
        assert!(rendered.contains("retry 1/3"));
        assert_eq!(lines.last(), Some(&String::new()));
    }

    #[test]
    fn empty_assistant_start_and_whitespace_deltas_do_not_move_the_transcript() {
        for starts_with_event in [true, false] {
            let view = view();
            view.add_user("hi");
            let before = view.chat.render(80);
            if starts_with_event {
                view.apply(&UiEvent::AssistantStart { blocks: vec![] }, 80);
                assert_eq!(view.chat.render(80), before);
            }
            for blocks in [
                vec![],
                vec![AssistantBlock::Text(" ".into())],
                vec![AssistantBlock::Thinking("\n".into())],
            ] {
                view.apply(
                    &UiEvent::AssistantUpdate {
                        blocks,
                        tool_calls: vec![],
                    },
                    80,
                );
                assert_eq!(view.chat.render(80), before);
            }
            view.apply(
                &UiEvent::AssistantUpdate {
                    blocks: vec![AssistantBlock::Text("hello".into())],
                    tool_calls: vec![],
                },
                80,
            );
            let visible = view.chat.render(80);
            assert!(visible.len() > before.len());
            assert!(visible.iter().any(|line| line.contains("hello")));
            assert_eq!(visible.last(), Some(&String::new()));
            view.apply(&UiEvent::AssistantEnd { blocks: vec![] }, 80);
            assert_eq!(view.chat.render(80), before);
            view.apply(&UiEvent::AssistantStart { blocks: vec![] }, 80);
            assert_eq!(view.chat.render(80), before);
        }
    }

    #[test]
    fn assistant_blocks_follow_content_order() {
        let message = json!({
            "content": [
                {"type": "thinking", "thinking": "why"},
                {"type": "text", "text": "hello"},
                {"type": "toolCall", "id": "c1", "name": "read", "arguments": {}},
            ]
        });
        assert_eq!(
            assistant_blocks_from_value(&message),
            vec![
                AssistantBlock::Thinking("why".into()),
                AssistantBlock::Text("hello".into()),
            ]
        );
    }

    #[test]
    fn assistant_blocks_decode_images() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[255, 0, 0])
                .unwrap();
        }
        let message = json!({
            "content": [{"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(&bytes), "mimeType": "image/png"}]
        });
        assert_eq!(
            assistant_blocks_from_value(&message),
            vec![AssistantBlock::Image(bytes.to_vec())]
        );
        let jpeg = rpi_tools::image_processing::process_image(
            &bytes,
            rpi_tools::image_processing::ImageProcessingOptions {
                format: Some(rpi_tools::image_processing::OutputFormat::Jpeg { quality: 85 }),
                ..Default::default()
            },
        )
        .unwrap();
        let blocks = assistant_blocks_from_value(&json!({
            "content": [{"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(jpeg), "mimeType": "image/jpeg"}]
        }));
        assert!(
            matches!(&blocks[0], AssistantBlock::Image(data) if data.starts_with(b"\x89PNG\r\n\x1a\n"))
        );
    }

    #[test]
    fn tool_calls_extract_ids_names_and_args() {
        let message = json!({
            "content": [
                {"type": "text", "text": "x"},
                {"type": "toolCall", "id": "b1", "name": "bash", "arguments": {"command": "ls"}},
                {"type": "toolCall", "id": "", "name": "read", "arguments": {}},
            ]
        });
        assert_eq!(
            tool_calls_from_value(&message),
            vec![UiToolCall {
                id: "b1".into(),
                name: "bash".into(),
                args: json!({"command": "ls"}),
            }]
        );
    }

    #[test]
    fn markdown_flag_is_opt_in() {
        assert!(tool_result_requests_markdown(Some(
            &json!({"markdown": true})
        )));
        assert!(!tool_result_requests_markdown(Some(
            &json!({"markdown": false})
        )));
        assert!(!tool_result_requests_markdown(Some(&json!({}))));
        assert!(!tool_result_requests_markdown(None));
    }

    #[test]
    fn skill_tool_name_only_matches_skill_reads() {
        assert_eq!(
            skill_tool_name("read", &json!({"path": "skills/foo/SKILL.md"})),
            Some("foo".to_string())
        );
        assert_eq!(skill_tool_name("read", &json!({"path": "README.md"})), None);
        assert_eq!(
            skill_tool_name("write", &json!({"path": "skills/foo/SKILL.md"})),
            None
        );
    }

    #[test]
    fn ask_user_args_display_never_leaks_transport_json() {
        let text = ask_user_args_display(&json!({
            "question": "你的代理端口是多少？",
            "context": "本机",
            "options": ["7890", {"title": "7897", "description": "clash"}],
            "suggest": "1080"
        }));
        assert!(text.contains("你的代理端口是多少？"));
        assert!(text.contains("本机"));
        assert!(text.contains("Choices: 7890, 7897"));
        assert!(text.contains("Suggestions: 1080"));
        assert!(!text.trim_start().starts_with('{'));
        assert_eq!(
            ask_user_args_display(&json!({"type": "confirm", "summary": "Deploy?"})),
            "Confirmation requested: Deploy?"
        );
    }

    #[test]
    fn tool_start_routes_bash_to_its_own_map() {
        let view = view();
        view.apply(
            &UiEvent::ToolStart {
                id: "b1".into(),
                name: "bash".into(),
                args: json!({"command": "ls -la"}),
            },
            80,
        );
        view.apply(
            &UiEvent::ToolStart {
                id: "r1".into(),
                name: "read".into(),
                args: json!({"path": "a.txt"}),
            },
            80,
        );
        assert!(view.bash_components.lock().unwrap().contains_key("b1"));
        assert!(view.tool_components.lock().unwrap().contains_key("r1"));
        assert!(view.tool_components.lock().unwrap().get("b1").is_none());
    }

    #[test]
    fn bash_start_without_a_command_is_deferred() {
        let view = view();
        view.apply(
            &UiEvent::ToolStart {
                id: "b1".into(),
                name: "bash".into(),
                args: json!({}),
            },
            80,
        );
        assert!(view.bash_components.lock().unwrap().is_empty());
        assert!(view.tool_components.lock().unwrap().is_empty());
    }

    #[test]
    fn assistant_stream_reuses_one_component() {
        let view = view();
        view.apply(&UiEvent::AssistantStart { blocks: vec![] }, 80);
        view.apply(
            &UiEvent::AssistantUpdate {
                blocks: vec![AssistantBlock::Text("hi".into())],
                tool_calls: vec![],
            },
            80,
        );
        assert!(view.current_assistant.lock().unwrap().is_some());
    }

    #[test]
    fn toggle_expanded_round_trips() {
        let view = view();
        assert!(!view.expanded());
        assert!(view.toggle_expanded());
        assert!(view.expanded());
    }

    #[test]
    fn todo_updates_share_one_panel_and_preserve_errors() {
        let view = view();
        for (id, done) in [("a", false), ("b", true)] {
            view.sync_tool_calls(&[UiToolCall {
                id: id.into(),
                name: "todo".into(),
                args: json!({"action":"toggle"}),
            }]);
            view.tool_start(id, "todo", &json!({}), 80);
            view.tool_end(id, "todo", &json!({"content":[],"details":{"todos":[{"id":1,"text":"Run tests","done":done}]}}), false, 80);
        }
        assert_eq!(view.chat.child_count(), 1);
        assert!(view.tool_components.lock().unwrap().is_empty());
        let children = view.chat.get_children();
        let panel = children[0]
            .as_any()
            .downcast_ref::<ToolExecutionComponent>()
            .unwrap();
        let rendered = rpi_tui::ansi::strip_ansi(&panel.render(80).join("\n"));
        assert!(rendered.contains("1/1 completed"));
        assert!(rendered.contains("[✓] Run tests"));
        view.tool_end(
            "error",
            "todo",
            &json!({"content":[{"type":"text","text":"invalid task"}]}),
            true,
            80,
        );
        assert_eq!(view.chat.child_count(), 2);
        view.tool_end("soft-error", "todo", &json!({"content":[{"type":"text","text":"todo not found"}],"details":{"todos":[],"error":"todo not found"}}), false, 80);
        assert_eq!(view.chat.child_count(), 3);
        assert!(update_todo_list(
            &view.chat,
            &json!({"details":{"todos":[]}})
        ));
        assert_eq!(view.chat.child_count(), 3);
        assert!(!update_todo_list(
            &view.chat,
            &json!({"details":{"todos":[{}]}})
        ));
        assert_eq!(view.chat.child_count(), 3);
    }

    #[test]
    fn completed_plan_keeps_its_transcript_position() {
        let view = view();
        view.apply(
            &UiEvent::AssistantStart {
                blocks: vec![AssistantBlock::Text("planning context".into())],
            },
            80,
        );
        view.apply(
            &UiEvent::AssistantEnd {
                blocks: vec![AssistantBlock::Text("planning context".into())],
            },
            80,
        );
        view.apply(
            &UiEvent::ToolEnd {
                id: "start".into(),
                name: "plan_mode_start".into(),
                result: json!({"details":{"kind":"plan","active":true}}),
                is_error: false,
            },
            80,
        );
        view.apply(
            &UiEvent::AssistantStart {
                blocks: vec![AssistantBlock::Text("repository findings".into())],
            },
            80,
        );
        view.apply(
            &UiEvent::AssistantEnd {
                blocks: vec![AssistantBlock::Text("repository findings".into())],
            },
            80,
        );

        view.apply(
            &UiEvent::ToolEnd {
                id: "complete".into(),
                name: "plan_mode_complete".into(),
                result: json!({
                    "details": {
                        "kind": "plan",
                        "active": false,
                        "plan": "1. Keep the plan in place"
                    }
                }),
                is_error: false,
            },
            80,
        );

        let lines: Vec<_> = view
            .chat
            .render(80)
            .iter()
            .map(|line| strip_ansi(line))
            .collect();
        let plan = lines
            .iter()
            .position(|line| line.contains("Plan · Ready"))
            .expect("completed plan panel should render");
        let findings = lines
            .iter()
            .position(|line| line.contains("repository findings"))
            .expect("later assistant output should render");
        assert!(
            plan < findings,
            "plan completion must not move the plan below later transcript output: {lines:?}"
        );
    }

    #[test]
    fn plan_tools_and_show_update_one_expandable_panel() {
        let view = view();
        view.sync_tool_calls(&[UiToolCall {
            id: "start".into(),
            name: "plan_mode_start".into(),
            args: json!({}),
        }]);
        view.tool_start("start", "plan_mode_start", &json!({}), 80);
        view.tool_end(
            "start",
            "plan_mode_start",
            &json!({"details":{"kind":"plan","active":true}}),
            false,
            80,
        );
        let body = (1..=30)
            .map(|i| format!("{i}. Step {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.tool_end(
            "complete",
            "plan_mode_complete",
            &json!({"details":{"kind":"plan","active":false,"plan":body}}),
            false,
            80,
        );
        assert_eq!(view.chat.child_count(), 1);
        assert!(view.tool_components.lock().unwrap().is_empty());
        view.set_expanded(true);
        let children = view.chat.get_children();
        let panel = children[0]
            .as_any()
            .downcast_ref::<ToolExecutionComponent>()
            .unwrap();
        assert!(rpi_tui::ansi::strip_ansi(&panel.render(80).join("\n")).contains("Step 30"));
        assert!(update_plan_panel(
            &view.chat,
            &json!({"kind":"message","details":{"kind":"plan","active":false,"plan":body,"expanded":true}})
        ));
        assert_eq!(view.chat.child_count(), 1);
        view.tool_end(
            "error",
            "plan_mode_complete",
            &json!({"content":[{"type":"text","text":"not active"}]}),
            true,
            80,
        );
        assert_eq!(view.chat.child_count(), 2);
    }
}
