//! Driver-generic TUI shell.
//!
//! The shell half of the unification: a single terminal UI that drives **any**
//! [`SessionDriver`] — chrome (banner + status slot + footer), the editor dock,
//! the input loop, and the slash-command surface. It renders the transcript
//! through the shared [`TranscriptView`] and applies normalized [`UiEvent`]s, so
//! the same shell serves the remote client today and the in-process host once
//! its runner adopts the driver seam.
//!
//! Local-only capabilities (session tree and extension management) are not part of
//! this shell: it is the transport-agnostic floor both frontends share.
//!
//! [`UiEvent`]: crate::transcript_view::UiEvent

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use rpi_tui::{
    bold as tui_bold, Component, Container, Editor, Focusable, FooterComponent, ProcessTerminal,
    ScrollView, StackChild, StackEntry, StatusIndicator, Text, TuiAltScreen, VStack, WorkingState,
    TUI,
};

use crate::session_driver::{DriverCommand, DriverEvent, DriverStatus, SessionDriver};
use crate::transcript_view::TranscriptView;

mod dialogs;
use dialogs::ShellDialogs;

/// Drive the generic shell against `driver` until the user exits (or the
/// driver's event stream ends). `addr` is a display label for the banner.
pub async fn run_driver_tui<D: SessionDriver>(driver: &D, addr: &str) -> Result<(), String> {
    let mut events = driver
        .events()
        .ok_or("driver did not yield an event stream")?;

    // ---- Build the UI ----
    let chrome = ChatChrome::new(addr);
    let document = Arc::new(Container::new());
    document.add_child(chrome.header.clone());
    let transcript_container = Arc::new(Container::new());
    document.add_child(transcript_container.clone());
    let scroll = Arc::new(ScrollView::primary(document.clone()));

    // Retained transcript view: the same pi-tui components the local TUI uses,
    // updated in place from the driver's normalized event stream.
    let view = TranscriptView::new(transcript_container.clone());

    let keybindings = crate::interactive_tui::configured_keybindings();
    let editor = Arc::new(Editor::new(
        rpi_tui::EditorOptions::default(),
        rpi_tui::EditorStyle::default(),
        keybindings.clone(),
    ));
    let editor_slot = Arc::new(Container::new());
    editor_slot.add_child(editor.clone());

    // Layout mirrors the local TUI's bottom dock: transcript (grow) over
    // status slot + editor + footer.
    let root = VStack::from_children(vec![
        StackChild::Entry(
            StackEntry::new(scroll.clone())
                .basis(0)
                .grow(1)
                .shrink(1)
                .min_size(1),
        ),
        StackChild::Entry(StackEntry::new(chrome.status.clone())),
        StackChild::Entry(StackEntry::new(editor_slot.clone()).shrink(0).min_size(3)),
        StackChild::Entry(StackEntry::new(chrome.footer.clone())),
    ]);
    let root: Arc<dyn Component> = Arc::new(root);

    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        false,
        None,
    ));
    crate::transcript_navigation::install(&tui, keybindings.clone());
    tui.set_layout_root(Some(root));
    tui.set_focus(Some(editor.clone()));
    editor.set_focused(true);
    tui.start_render_scheduler();
    tui.start_readerless();

    // ---- Input plumbing ----
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    editor.on_submit(Arc::new(move |text: &str| {
        let _ = submit_tx.send(text.to_string());
    }));

    let reader_stop = tokio_util::sync::CancellationToken::new();
    let stop_reader = reader_stop.clone();
    let reader = tokio::task::spawn_blocking(move || {
        while !stop_reader.is_cancelled() {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            match crossterm::event::read() {
                Ok(event) => {
                    // The size is adopted from `Event::Resize` in the run loop
                    // below. Refreshing on every event used to invalidate the
                    // painted frame on each keystroke (a full repaint per key)
                    // without ever learning a new size.
                    if input_tx.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Initial paint (the transcript starts empty; chrome shows connection state).
    let mut status = DriverStatus::default();
    chrome.refresh(addr, &status);
    tui.request_render(true);

    let outcome = run_loop(
        driver,
        &mut status,
        &mut events,
        &mut input_rx,
        &mut submit_rx,
        &tui,
        &editor,
        &editor_slot,
        &chrome,
        &view,
        &keybindings,
        addr,
    )
    .await;

    reader_stop.cancel();
    let _ = reader.await;
    tui.stop(Default::default());
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn run_loop<D: SessionDriver>(
    driver: &D,
    status: &mut DriverStatus,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<DriverEvent>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    submit_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    tui: &Arc<TuiAltScreen>,
    editor: &Arc<Editor>,
    editor_slot: &Arc<Container>,
    chrome: &ChatChrome,
    view: &TranscriptView,
    keybindings: &rpi_tui::Keybindings,
    addr: &str,
) -> Result<(), String> {
    // Repaint cadence while a run is in flight so the in-editor spinner and the
    // retry countdown animate between streamed tokens (mirrors the local render
    // tick). Idle screens never request a frame.
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (dialog_tx, mut dialog_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut dialogs = ShellDialogs::new(dialog_tx);
    loop {
        dialogs.sync_view(editor_slot, editor, tui);
        tokio::select! {
            maybe_event = events.recv() => {
                let Some(event) = maybe_event else { break };
                match event {
                    DriverEvent::UiDialog(request) => dialogs.push(request),
                    DriverEvent::UiDialogClosed(id) => dialogs.close(&id),
                    DriverEvent::Ui(ui) => {
                        if matches!(ui, crate::transcript_view::UiEvent::AgentEnd) { let _ = dialogs.clear(); }
                        view.apply(&ui, tui.width());
                    }
                    DriverEvent::Status(next) => {
                        *status = next;
                        sync_working(editor, status.streaming);
                        chrome.refresh(addr, status);
                    }
                    DriverEvent::Notice(message) => view.add_notice(&message),
                    DriverEvent::Ended => break,
                }
                tui.request_render(false);
            }
            maybe_input = input_rx.recv() => {
                let Some(event) = maybe_input else { break };
                match event {
                    Event::Resize(..) => {
                        tui.note_resize();
                    }
                    Event::Mouse(mouse) => {
                        if tui.handle_scroll_to_end_indicator_mouse_event(&mouse) { continue; }
                        match mouse.kind {
                            MouseEventKind::ScrollUp => tui.scroll_by(-3),
                            MouseEventKind::ScrollDown => tui.scroll_by(3),
                            _ => {}
                        }
                        tui.request_render(false);
                    }
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        crate::key_trace::key(&key, "driver-tui");
                        if is_ctrl(&key, 'c') {
                            let had_dialog = dialogs.has_active();
                            for cancel in dialogs.clear() {
                                let _ = driver.send(cancel).await;
                            }
                            if status.streaming {
                                view.add_notice("aborting…");
                                let _ = driver.send(DriverCommand::Abort).await;
                            } else if !had_dialog {
                                break;
                            }
                        } else if !dialogs.handle_key(key)
                            && !crate::transcript_navigation::handle_key(tui, keybindings, &key) {
                            editor.handle_key(key);
                        }
                        tui.request_render(false);
                    }
                    Event::Paste(text) => {
                        if !dialogs.handle_paste(&text) { editor.insert(&text); }
                        tui.request_render(false);
                    }
                    other => {
                        crate::key_trace::note(&format!("driver tui event {other:?}"));
                    }
                }
            }
            maybe_text = submit_rx.recv() => {
                let Some(text) = maybe_text else { break };
                // A normal editor submission queued before a question arrived
                // must not bypass the active approval UI.
                if dialogs.has_active() { continue; }
                match handle_submit(driver, status, view, &text).await {
                    Ok(SubmitAction::Continue) => {}
                    Ok(SubmitAction::Quit) => break,
                    Err(error) => view.add_notice(&format!("error: {error}")),
                }
                editor.clear();
                tui.request_render(false);
            }
            Some(action) = dialog_rx.recv() => {
                if let Some((id, command)) = dialogs.action(action) {
                    match driver.send(command).await {
                        Ok(()) => dialogs.close(&id),
                        Err(error) => view.add_notice(&format!("error: {error}")),
                    }
                }
            }
            _ = ticker.tick() => {
                // Size watchdog: an event can be missed (or arrive before the
                // window rectangle moves), and a fullscreen frame must always
                // match the real window.
                tui.ensure_size();
                let retrying = chrome.tick_retry();
                if status.streaming || retrying {
                    tui.request_render(false);
                }
            }
            else => break,
        }
    }
    for cancel in dialogs.clear() {
        let _ = driver.send(cancel).await;
    }
    Ok(())
}

enum SubmitAction {
    Continue,
    Quit,
}

async fn handle_submit<D: SessionDriver>(
    driver: &D,
    status: &DriverStatus,
    view: &TranscriptView,
    text: &str,
) -> Result<SubmitAction, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(SubmitAction::Continue);
    }

    if let Some(rest) = trimmed.strip_prefix('/') {
        let (name, arg) = match rest.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (rest, ""),
        };
        match name {
            "exit" | "quit" | "q" => return Ok(SubmitAction::Quit),
            "abort" => driver.send(DriverCommand::Abort).await?,
            "state" => driver.send(DriverCommand::RefreshState).await?,
            "model" if !arg.is_empty() => {
                driver
                    .send(DriverCommand::SetModel(arg.to_string()))
                    .await?
            }
            "thinking" if !arg.is_empty() => {
                driver
                    .send(DriverCommand::SetThinkingLevel(arg.to_string()))
                    .await?
            }
            "tools" => {
                let tools = if status.active_tools.is_empty() {
                    "(default)".to_string()
                } else {
                    status.active_tools.join(", ")
                };
                view.add_notice(&format!("active tools: {tools}"));
            }
            "help" => view
                .add_notice("commands: /abort /state /model <id> /thinking <level> /tools /exit"),
            "" => view.add_notice("empty command (try /help)"),
            other => view.add_notice(&format!(
                "unsupported command: /{other} (session-tree and reload commands are not available in remote mode; try /help)"
            )),
        }
        return Ok(SubmitAction::Continue);
    }

    view.add_user(trimmed);
    driver
        .send(DriverCommand::Prompt(trimmed.to_string()))
        .await?;
    Ok(SubmitAction::Continue)
}

fn is_ctrl(key: &KeyEvent, ch: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(ch)
}

/// The non-transcript chrome (top banner + status slot + footer), built from
/// the same `pi-tui` components the local TUI uses ([`StatusIndicator`],
/// [`FooterComponent`]) so the two screens read the same.
struct ChatChrome {
    header: Arc<Text>,
    status: Arc<Container>,
    footer: Arc<FooterComponent>,
    /// The live retry indicator, kept across refreshes so its countdown ticks.
    retry: Mutex<Option<(u32, u32, Arc<StatusIndicator>)>>,
}

impl ChatChrome {
    fn new(addr: &str) -> Self {
        let footer = Arc::new(FooterComponent::new());
        footer.set_hints(
            "Enter: Send | Shift+Enter: New line | Ctrl+C: Clear/Exit | Esc: Abort | /help",
        );
        Self {
            header: Arc::new(Text::new(format!("rpi · {addr} · connecting…"), 1, 0)),
            status: Arc::new(Container::new()),
            footer,
            retry: Mutex::new(None),
        }
    }

    /// Refresh header + footer + retry indicator from the driver status.
    fn refresh(&self, addr: &str, status: &DriverStatus) {
        let model = status.model.as_deref().unwrap_or("");
        let thinking = status.thinking.as_deref().unwrap_or("off");
        let connection = if status.ready { "ready" } else { "connecting" };
        self.header.set_text(tui_bold(&format!(
            "rpi · {addr} · {connection} · model {} · thinking {thinking}",
            if model.is_empty() { "(default)" } else { model }
        )));
        self.footer.set_model(&short_model_name(model));
        self.footer.set_thinking_level(Some(thinking));
        self.footer.set_status("");

        let mut slot = self.retry.lock().unwrap();
        match status.retry {
            Some((attempt, max, delay_ms)) => {
                let unchanged =
                    matches!(slot.as_ref(), Some((a, m, _)) if *a == attempt && *m == max);
                if !unchanged {
                    let indicator = Arc::new(StatusIndicator::retry(
                        attempt,
                        max,
                        Duration::from_millis(delay_ms),
                    ));
                    self.status.clear();
                    self.status.add_child(indicator.clone());
                    *slot = Some((attempt, max, indicator));
                }
            }
            None => {
                if slot.take().is_some() {
                    self.status.clear();
                }
            }
        }
    }

    /// Tick the retry countdown; returns whether a retry is currently shown.
    fn tick_retry(&self) -> bool {
        match self.retry.lock().unwrap().as_ref() {
            Some((_, _, indicator)) => {
                indicator.tick_countdown();
                true
            }
            None => false,
        }
    }
}

/// Keep the in-editor working indicator in step with the run state — the local
/// TUI shows the spinner in the editor's top border, not the status slot.
fn sync_working(editor: &Arc<Editor>, streaming: bool) {
    if streaming {
        if editor.working().is_none() {
            editor.set_working(Some(WorkingState {
                started_at: Instant::now(),
                message: "Working…".to_string(),
            }));
        }
    } else if editor.working().is_some() {
        editor.set_working(None);
    }
}

/// The trailing path segment of a model id (`provider/model` → `model`).
fn short_model_name(id: &str) -> String {
    id.rsplit([':', '/'])
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(id)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_detection_is_specific() {
        assert!(is_ctrl(
            &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            'c'
        ));
        assert!(!is_ctrl(
            &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            'c'
        ));
    }

    #[test]
    fn short_model_name_keeps_the_trailing_segment() {
        assert_eq!(short_model_name("openai/gpt-5.2"), "gpt-5.2");
        assert_eq!(short_model_name("anthropic:claude-4"), "claude-4");
        assert_eq!(short_model_name("bare"), "bare");
        assert_eq!(short_model_name(""), "");
    }
}
