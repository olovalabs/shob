//! Native side-chat workspace. No messages or drafts are shared with the main lane.
use super::*;
use rpi_extensions::RuntimeActionHost;
use rpi_tui::HStack;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Weak;

pub(super) fn input_allowed(state: &TuiState) -> bool {
    !state.selector_open()
        && !state.extension_dialog_open()
        && state.provider_form.lock().unwrap().is_none()
        && state.active_provider_input.lock().unwrap().is_none()
        && state.active_provider_select.lock().unwrap().is_none()
        && state.provider_auth_flow_id.load(Ordering::Acquire) == 0
}
fn safe_text(text: &str) -> String {
    rpi_tui::strip_ansi(text)
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
struct TurnView {
    user: Arc<UserMessageComponent>,
    assistant: Arc<AssistantMessageComponent>,
    error: Arc<Text>,
    text: String,
}
struct View {
    owner: u64,
    id: String,
    root: Arc<VStack>,
    content: Arc<Container>,
    scroll: Arc<ScrollView>,
    editor: Arc<Editor>,
    header: Arc<Text>,
    queued: Arc<Text>,
    notice: Arc<Text>,
    turns: Mutex<HashMap<String, TurnView>>,
    submitting: AtomicBool,
    host: Arc<dyn RuntimeActionHost>,
    runtime: tokio::runtime::Handle,
    tui: Weak<TuiAltScreen>,
    show_terminal_progress: bool,
}
impl View {
    fn new(
        owner: u64,
        id: String,
        host: Arc<dyn RuntimeActionHost>,
        runtime: tokio::runtime::Handle,
        tui: &Arc<TuiAltScreen>,
        keys: Arc<rpi_tui::keybindings::Keybindings>,
        show_terminal_progress: bool,
        footer: Arc<FooterComponent>,
        footer_panels: Arc<dyn Component>,
    ) -> Arc<Self> {
        let header = Arc::new(Text::new("BTW", 0, 0));
        let queued = Arc::new(Text::new("", 0, 0));
        let notice = Arc::new(Text::new("", 0, 0));
        let content = Arc::new(Container::new());
        let scroll = Arc::new(ScrollView::new(
            content.clone(),
            ScrollViewOptions {
                follow: FollowMode::End,
                primary: false,
                overscroll: OverscrollMode::Contain,
                scrollbar: ScrollbarMode::Always,
                preserve_manual_position: true,
                ..Default::default()
            },
        ));
        let editor = Arc::new(Editor::new(
            EditorOptions::default(),
            EditorStyle::default(),
            keys,
        ));
        let root = Arc::new(VStack::from_entries(vec![
            StackEntry::new(header.clone()).shrink(0).max_size(1),
            StackEntry::new(scroll.clone()).basis(0).grow(1).min_size(1),
            StackEntry::new(queued.clone()).max_size(3),
            StackEntry::new(notice.clone()).max_size(2),
            StackEntry::new(Arc::new(Text::new(
                "Enter ask · PgUp/PgDn · Ctrl+End latest · Esc hide · Ctrl+C cancel",
                0,
                0,
            )))
            .shrink(0)
            .max_size(1),
            StackEntry::new(editor.clone()).min_size(3).max_size(8),
            StackEntry::new(footer.clone()),
            StackEntry::new(footer_panels.clone()),
        ]));
        let view = Arc::new(Self {
            owner,
            id,
            root,
            content,
            scroll,
            editor,
            header,
            queued,
            notice,
            turns: Mutex::new(HashMap::new()),
            submitting: AtomicBool::new(false),
            host,
            runtime,
            tui: Arc::downgrade(tui),
            show_terminal_progress,
        });
        let weak = Arc::downgrade(&view);
        view.editor.on_submit(Arc::new(move |_| {
            if let Some(view) = weak.upgrade() {
                view.submit();
            }
        }));
        view
    }
    fn submit(self: &Arc<Self>) {
        let question = self.editor.get_expanded_text().trim().to_owned();
        if question.is_empty() {
            return;
        }
        if self.submitting.swap(true, Ordering::AcqRel) {
            return;
        }
        let view = self.clone();
        self.runtime.spawn(async move {
            let result = view
                .host
                .side_question_ui(
                    view.owner,
                    view.id.clone(),
                    json!({"op":"start","question":question}),
                )
                .await;
            match result {
                Ok(result) => {
                    // Don't erase edits made while admission was pending.
                    if view.editor.get_expanded_text().trim() == question {
                        view.editor.clear();
                    }
                    view.notice.set_text(if result["status"] == "queued" {
                        "Question queued."
                    } else {
                        ""
                    });
                }
                Err(error) => view.notice.set_text(safe_text(&error)),
            }
            view.submitting.store(false, Ordering::Release);
            if let Some(tui) = view.tui.upgrade() {
                tui.request_render(false);
            }
        });
    }
    fn action(self: &Arc<Self>, op: &'static str) {
        let view = self.clone();
        self.runtime.spawn(async move {
            if let Err(error) = view
                .host
                .side_question_ui(view.owner, view.id.clone(), json!({"op":op}))
                .await
            {
                view.notice.set_text(safe_text(&error));
            }
            if let Some(tui) = view.tui.upgrade() {
                tui.request_render(false);
            }
        });
    }
    fn update(&self, snapshot: &Value) {
        let mut cached = self.turns.lock().unwrap();
        let turns = snapshot["turns"].as_array().cloned().unwrap_or_default();
        cached.retain(|id, _| turns.iter().any(|t| t["requestId"].as_str() == Some(id)));
        self.content.clear();
        for turn in &turns {
            let id = turn["requestId"].as_str().unwrap_or("").to_owned();
            let item = cached.entry(id).or_insert_with(|| TurnView {
                user: Arc::new(UserMessageComponent::new(safe_text(
                    turn["question"].as_str().unwrap_or(""),
                ))),
                assistant: {
                    let assistant = Arc::new(AssistantMessageComponent::default());
                    assistant.set_trailing_spacing(1);
                    assistant
                },
                error: Arc::new(Text::new("", 1, 0)),
                text: String::new(),
            });
            let text = safe_text(turn["text"].as_str().unwrap_or(""));
            if text != item.text {
                item.assistant.update_text(&text);
                item.text = text;
            }
            item.assistant.set_streaming(turn["status"] == "running");
            item.error
                .set_text(safe_text(turn["error"].as_str().unwrap_or("")));
            self.content.add_child(item.user.clone());
            self.content.add_child(item.assistant.clone());
            if turn["error"].as_str().is_some() {
                self.content.add_child(item.error.clone());
            }
        }
        if turns.is_empty() {
            self.content.add_child(Arc::new(Text::new(
                "Ask a side question below. The main task continues independently.",
                1,
                1,
            )));
        }
        let questions = snapshot["queued"].as_array().cloned().unwrap_or_default();
        let queued = questions
            .iter()
            .take(2)
            .map(|q| safe_text(q.as_str().unwrap_or("")).replace('\n', " "))
            .collect::<Vec<_>>();
        self.queued.set_text(if queued.is_empty() {
            String::new()
        } else {
            format!("Queued ({}): {}", questions.len(), queued.join(" · "))
        });
        let status = snapshot["status"].as_str().unwrap_or("idle");
        let running = status == "running";
        if self.show_terminal_progress && running {
            if self.editor.working().is_none() {
                self.editor.set_working(Some(WorkingState {
                    started_at: std::time::Instant::now(),
                    message: "Working…".to_string(),
                }));
            }
        } else if self.editor.working().is_some() {
            self.editor.set_working(None);
        }
        let history = if snapshot["historyTruncated"] == true {
            " · recent history only"
        } else {
            ""
        };
        self.header.set_text(format!("BTW · {status}{history}"));
    }
    fn key(self: &Arc<Self>, key: KeyEvent) {
        match (key.modifiers, key.code) {
            // Editor::submit clears immediately; admission can fail or take time.
            // Keep this draft until the host actually accepts the question.
            (KeyModifiers::NONE, KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r')) => {
                self.submit()
            }
            (KeyModifiers::NONE, KeyCode::Esc) => self.action("hide"),
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.action("cancel"),
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                self.scroll
                    .scroll_by(-(self.scroll.viewport_height().max(1) as i32));
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                self.scroll
                    .scroll_by(self.scroll.viewport_height().max(1) as i32);
            }
            (KeyModifiers::CONTROL, KeyCode::Home) => self.scroll.scroll_to_start(),
            (KeyModifiers::CONTROL, KeyCode::End) => self.scroll.scroll_to_end(),
            _ => {
                self.editor.handle_key(key);
            }
        }
    }
}
/// Regular terminal mode has no constrained root layout. Render this one native
/// stack at the terminal's actual height rather than printing all transcript rows.
struct RegularView {
    view: Arc<View>,
    tui: Weak<TuiAltScreen>,
}
impl Component for RegularView {
    fn render(&self, width: usize) -> Vec<String> {
        let height = self
            .tui
            .upgrade()
            .map(|t| t.terminal().rows())
            .unwrap_or(24);
        rpi_tui::layout::render_layout_frame(self.view.root.clone(), width, height).lines
    }
    fn invalidate(&self) {
        self.view.root.invalidate();
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(super) struct SideQuestionUi {
    workspace: Arc<HStack>,
    main: Arc<dyn Component>,
    main_editor: Arc<Editor>,
    views: HashMap<String, Arc<View>>,
    active: Option<String>,
    focused: bool,
    suspended: bool,
    regular: bool,
    show_terminal_progress: bool,
    footer: Arc<FooterComponent>,
    footer_panels: Arc<dyn Component>,
    revision: Option<u64>,
    session_id: String,
    focus_revision: u64,
    geometry: Option<(usize, bool, Option<String>)>,
}
impl SideQuestionUi {
    pub(super) fn new(
        main: Arc<dyn Component>,
        editor: Arc<Editor>,
        regular: bool,
        show_terminal_progress: bool,
        footer: Arc<FooterComponent>,
        footer_panels: Arc<dyn Component>,
    ) -> Self {
        let workspace = Arc::new(HStack::from_entries(vec![StackEntry::new(main.clone())
            .basis(0)
            .grow(1)
            .min_size(1)]));
        Self {
            workspace,
            main,
            main_editor: editor,
            views: HashMap::new(),
            active: None,
            focused: false,
            suspended: false,
            regular,
            show_terminal_progress,
            footer,
            footer_panels,
            revision: None,
            session_id: String::new(),
            focus_revision: 0,
            geometry: None,
        }
    }
    pub(super) fn root(&self) -> Arc<HStack> {
        self.workspace.clone()
    }
    fn view(&self) -> Option<Arc<View>> {
        self.active
            .as_ref()
            .and_then(|id| self.views.get(id))
            .cloned()
    }
    pub(super) fn focused(&self) -> bool {
        self.focused && !self.suspended && self.active.is_some()
    }
    pub(super) fn empty_draft(&self) -> bool {
        self.view()
            .map_or(true, |view| view.editor.get_expanded_text().is_empty())
    }
    fn focus(&mut self, side: bool, tui: &Arc<TuiAltScreen>) {
        self.focused = side;
        self.main_editor.set_focused(!side);
        if let Some(view) = self.view() {
            view.editor.set_focused(side);
            tui.set_focus(Some(if side {
                view.editor.clone()
            } else {
                self.main_editor.clone()
            }));
        } else {
            tui.set_focus(Some(self.main_editor.clone()));
            self.main_editor.set_focused(true);
        }
    }
    pub(super) fn sync(
        &mut self,
        tui: &Arc<TuiAltScreen>,
        ctx: &crate::session::ReloadContext,
        parent: &AgentHarness,
        keys: Arc<rpi_tui::keybindings::Keybindings>,
        allowed: bool,
    ) -> bool {
        let host = ctx
            .action_bridge
            .lock()
            .unwrap()
            .as_ref()
            .map(|b| b.clone_host());
        self.sync_host(
            tui,
            host,
            ctx.runtime.clone(),
            parent.session().storage().metadata().id.clone(),
            keys,
            allowed,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn sync_host(
        &mut self,
        tui: &Arc<TuiAltScreen>,
        host: Option<Arc<dyn RuntimeActionHost>>,
        runtime: tokio::runtime::Handle,
        session_id: String,
        keys: Arc<rpi_tui::keybindings::Keybindings>,
        allowed: bool,
    ) -> bool {
        let revision = host
            .as_ref()
            .map(|h| h.side_question_revision())
            .unwrap_or(0);
        let mut changed = false;
        if self.revision != Some(revision) || self.session_id != session_id {
            self.revision = Some(revision);
            self.session_id = session_id.clone();
            let snapshots = host
                .as_ref()
                .map(|h| h.side_question_views())
                .unwrap_or_default();
            self.views.retain(|id, _| {
                snapshots
                    .iter()
                    .any(|s| s["threadId"].as_str() == Some(id) && s["sessionId"] == session_id)
            });
            let selected = snapshots
                .iter()
                .filter(|s| s["visible"] == true && s["sessionId"] == session_id)
                .max_by_key(|s| s["focusRevision"].as_u64().unwrap_or(0));
            let next = selected
                .and_then(|s| s["threadId"].as_str())
                .map(str::to_owned);
            let next_focus = selected
                .and_then(|s| s["focusRevision"].as_u64())
                .unwrap_or(0);
            for s in snapshots.iter().filter(|s| s["sessionId"] == session_id) {
                let id = s["threadId"].as_str().unwrap_or("").to_owned();
                let view = self.views.entry(id.clone()).or_insert_with(|| {
                    View::new(
                        s["owner"].as_u64().unwrap_or(0),
                        id,
                        host.as_ref().unwrap().clone(),
                        runtime.clone(),
                        tui,
                        keys.clone(),
                        self.show_terminal_progress,
                        self.footer.clone(),
                        self.footer_panels.clone(),
                    )
                });
                view.update(s);
            }
            let selected_changed = self.active != next;
            if selected_changed {
                if let Some(view) = self.view() {
                    view.editor.set_focused(false);
                }
            }
            self.active = next;
            if allowed && (selected_changed || next_focus > self.focus_revision) {
                self.focus(self.active.is_some(), tui);
            }
            self.focus_revision = next_focus;
            changed = true;
        }
        let suspended = !allowed;
        let resuming = self.suspended && !suspended;
        self.suspended = suspended;
        let width = tui.width();
        let side_only = self.regular || width < 100;
        if resuming {
            self.focus(self.active.is_some() && self.focused, tui);
        }
        if self.active.is_some() && !suspended && side_only && !self.focused {
            self.focus(true, tui);
        }
        let geometry = (width, suspended, self.active.clone());
        if self.geometry.as_ref() != Some(&geometry) {
            self.workspace.clear();
            if let Some(view) = self.view().filter(|_| !suspended) {
                if !side_only {
                    self.workspace.add_child_with_options(
                        self.main.clone(),
                        rpi_tui::StackEntryOptions {
                            basis: Some(width / 2),
                            shrink: 0,
                            ..Default::default()
                        },
                    );
                    self.workspace.add_child_with_options(
                        Arc::new(Text::new("│", 0, 0)),
                        rpi_tui::StackEntryOptions {
                            basis: Some(1),
                            shrink: 0,
                            ..Default::default()
                        },
                    );
                }
                let side: Arc<dyn Component> = if self.regular {
                    Arc::new(RegularView {
                        view,
                        tui: Arc::downgrade(tui),
                    })
                } else {
                    view.root.clone()
                };
                self.workspace.add_child_with_options(
                    side,
                    rpi_tui::StackEntryOptions {
                        basis: Some(0),
                        grow: 1,
                        min_size: 1,
                        ..Default::default()
                    },
                );
            } else {
                self.workspace.add_child_with_options(
                    self.main.clone(),
                    rpi_tui::StackEntryOptions {
                        basis: Some(0),
                        grow: 1,
                        min_size: 1,
                        ..Default::default()
                    },
                );
            }
            self.geometry = Some(geometry);
            changed = true;
        }
        let side_animating = !suspended
            && self
                .view()
                .is_some_and(|view| view.editor.working().is_some());
        changed || side_animating
    }
    pub(super) fn key(&mut self, key: KeyEvent, tui: &Arc<TuiAltScreen>) -> bool {
        if !self.focused() {
            return false;
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('z') {
            return false;
        }
        if let Some(view) = self.view() {
            view.key(key);
        }
        tui.request_render(false);
        true
    }
    pub(super) fn paste(&self, text: &str, tui: &Arc<TuiAltScreen>) -> bool {
        if !self.focused() {
            return false;
        }
        if let Some(view) = self.view() {
            view.editor.handle_paste(text);
        }
        tui.request_render(false);
        true
    }
    pub(super) fn mouse(
        &mut self,
        event: &crossterm::event::MouseEvent,
        tui: &Arc<TuiAltScreen>,
        main_scroll: &ScrollView,
    ) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        if self.suspended {
            return false;
        }
        let Some(view) = self.view() else {
            return false;
        };
        let width = tui.width();
        let side_only = self.regular || width < 100;
        let over_side = side_only || event.column as usize > width / 2;
        match event.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if event.kind == MouseEventKind::ScrollUp {
                    -MOUSE_WHEEL_SCROLL_LINES
                } else {
                    MOUSE_WHEEL_SCROLL_LINES
                };
                if over_side {
                    view.scroll.scroll_by(delta);
                } else {
                    main_scroll.scroll_by(delta);
                }
                tui.request_render(false);
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus(over_side, tui);
                tui.request_render(false);
                // Main's existing selection code assumes full-width coordinates.
                true
            }
            MouseEventKind::Down(MouseButton::Right) if over_side && cfg!(windows) => {
                if let Some(text) = read_clipboard_text() {
                    view.editor.handle_paste(&text);
                }
                tui.request_render(false);
                true
            }
            _ => over_side,
        }
    }
}

#[cfg(test)]
mod tests;
