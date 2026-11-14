//! Terminal event plumbing.
//!
//! `alacritty_terminal` talks to the embedder through an [`EventListener`].
//! A surprising amount of terminal functionality is implemented as a *request*
//! that the embedder is expected to answer by writing bytes back into the PTY:
//!
//! * `Event::PtyWrite` — cursor position reports (`CSI 6 n`), device attributes
//!   (`CSI c`), DECRQM replies, XTVERSION, kitty keyboard queries, …
//! * `Event::ColorRequest` — `OSC 4 / 10 / 11 / 12` colour queries.
//! * `Event::TextAreaSizeRequest` — `CSI 14 t` / `CSI 18 t` size queries.
//! * `Event::ClipboardLoad` — `OSC 52` clipboard reads.
//!
//! If those go unanswered, full screen TUIs (vim, htop, lazygit, fzf, tmux,
//! neovim, ratatui apps, AI agent CLIs, …) either hang waiting for a reply or
//! silently fall back to a degraded rendering path. [`GpuiEventProxy`] answers
//! the synchronous ones inline through a [`TerminalNotifier`] and forwards the
//! ones that need main-thread/UI access (clipboard, bell, title, …) over a
//! channel to [`crate::view::TerminalView`].

use crate::colors::ColorPalette;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::term::ClipboardType;
use alacritty_terminal::vte::ansi::Rgb;
use parking_lot::Mutex;
use std::fmt;
use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::Sender;

/// Standalone synchronous writer, retained for hosts constructing their own notifier.
pub type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Formatter used by alacritty to turn a clipboard payload into the escape
/// sequence the requesting application expects.
pub type ClipboardFormatter = Arc<dyn Fn(&str) -> String + Sync + Send + 'static>;

/// Everything the event proxy needs in order to answer a terminal query
/// without touching the UI thread.
#[derive(Clone)]
pub struct TerminalNotifier {
    writer: NotifierWriter,
    window_size: Arc<Mutex<WindowSize>>,
    palette: Arc<Mutex<ColorPalette>>,
}

#[derive(Clone)]
enum NotifierWriter {
    Standalone(PtyWriter),
    Queued(crate::pty_writer::QueuedWriter),
}

impl TerminalNotifier {
    pub fn new(
        writer: PtyWriter,
        window_size: Arc<Mutex<WindowSize>>,
        palette: Arc<Mutex<ColorPalette>>,
    ) -> Self {
        Self {
            writer: NotifierWriter::Standalone(writer),
            window_size,
            palette,
        }
    }

    pub(crate) fn queued(
        writer: crate::pty_writer::QueuedWriter,
        window_size: Arc<Mutex<WindowSize>>,
        palette: Arc<Mutex<ColorPalette>>,
    ) -> Self {
        Self {
            writer: NotifierWriter::Queued(writer),
            window_size,
            palette,
        }
    }

    /// Write raw bytes into the PTY.
    ///
    /// TerminalView replies enter the same bounded FIFO as user input; only
    /// standalone notifiers constructed with `new` perform synchronous I/O.
    pub fn write(&self, bytes: &[u8]) {
        match &self.writer {
            NotifierWriter::Standalone(writer) => {
                let _ = writer.lock().write_all(bytes);
            }
            NotifierWriter::Queued(writer) => {
                // Admission failures are delivered to the view's independent error task.
                let _ = writer.send(bytes);
            }
        }
    }

    pub fn write_str(&self, text: &str) {
        self.write(text.as_bytes());
    }

    pub fn window_size(&self) -> WindowSize {
        *self.window_size.lock()
    }

    pub fn color(&self, index: usize) -> Rgb {
        self.palette.lock().rgb_at(index)
    }
}

/// Events that need to be handled on the UI thread.
#[derive(Clone)]
pub enum TerminalEvent {
    /// New content is available; the view should repaint.
    Wakeup,

    /// `BEL` was received.
    Bell,

    /// `OSC 0 / 1 / 2` window/icon title change.
    Title(String),

    /// `OSC 2` reset — the title should fall back to the default.
    ResetTitle,

    /// `OSC 52` clipboard write request.
    ClipboardStore(ClipboardType, String),

    /// `OSC 52` clipboard read request. The payload must be handed to the
    /// formatter and the result written back into the PTY.
    ClipboardLoad(ClipboardType, ClipboardFormatter),

    /// `DECSCUSR` / `CSI ? 12 h|l` changed whether the cursor blinks.
    CursorBlinkingChange,

    /// The grid changed in a way that may require a different mouse cursor.
    MouseCursorDirty,

    /// The child process exited with the given status code.
    ChildExit(i32),

    /// The terminal wants to shut down (EOF on the PTY, `Event::Exit`, …).
    Exit,
}

impl fmt::Debug for TerminalEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TerminalEvent::Wakeup => write!(f, "Wakeup"),
            TerminalEvent::Bell => write!(f, "Bell"),
            TerminalEvent::Title(title) => write!(f, "Title({title})"),
            TerminalEvent::ResetTitle => write!(f, "ResetTitle"),
            TerminalEvent::ClipboardStore(ty, text) => write!(f, "ClipboardStore({ty:?}, {text})"),
            TerminalEvent::ClipboardLoad(ty, _) => write!(f, "ClipboardLoad({ty:?})"),
            TerminalEvent::CursorBlinkingChange => write!(f, "CursorBlinkingChange"),
            TerminalEvent::MouseCursorDirty => write!(f, "MouseCursorDirty"),
            TerminalEvent::ChildExit(code) => write!(f, "ChildExit({code})"),
            TerminalEvent::Exit => write!(f, "Exit"),
        }
    }
}

/// Slot the [`TerminalView`](crate::view::TerminalView) uses to hand the PTY
/// writer to the proxy *after* the terminal has been constructed.
pub type NotifierSlot = Arc<Mutex<Option<TerminalNotifier>>>;

/// Cheap, clonable handle to the shared PTY notifier slot.
///
/// `GpuiEventProxy` itself is not `Clone` (its event channel is not), but the
/// notifier lives behind an `Arc`, so this handle can be stored anywhere.
/// A write before the host installs the notifier is dropped, which is also
/// what the inline path does.
#[derive(Clone)]
pub struct TerminalNotifierHandle {
    notifier: NotifierSlot,
}

impl TerminalNotifierHandle {
    /// Write raw bytes into the installed notifier's FIFO. No-op until the
    /// host installs the PTY notifier via `notifier_slot`.
    pub fn write(&self, bytes: &[u8]) {
        if let Some(notifier) = self.notifier.lock().as_ref() {
            notifier.write(bytes);
        }
    }
}

pub struct GpuiEventProxy {
    tx: Sender<TerminalEvent>,
    notifier: NotifierSlot,
}

impl GpuiEventProxy {
    pub fn new(tx: Sender<TerminalEvent>) -> Self {
        Self {
            tx,
            notifier: Arc::new(Mutex::new(None)),
        }
    }

    /// Handle used to install the PTY notifier once the writer exists.
    pub fn notifier_slot(&self) -> NotifierSlot {
        Arc::clone(&self.notifier)
    }

    /// Cheap, clonable write-side handle for embedders that answer protocol
    /// queries outside the normal event flow (e.g. the XTGETTCAP passthrough).
    pub fn notifier_handle(&self) -> TerminalNotifierHandle {
        TerminalNotifierHandle {
            notifier: Arc::clone(&self.notifier),
        }
    }

    fn send(&self, event: TerminalEvent) {
        let _ = self.tx.send(event);
    }

    fn with_notifier<F: FnOnce(&TerminalNotifier)>(&self, f: F) {
        let guard = self.notifier.lock();
        if let Some(notifier) = guard.as_ref() {
            f(notifier);
        }
    }
}

impl EventListener for GpuiEventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => self.send(TerminalEvent::Wakeup),
            Event::Bell => self.send(TerminalEvent::Bell),
            Event::Title(title) => self.send(TerminalEvent::Title(title)),
            Event::ResetTitle => self.send(TerminalEvent::ResetTitle),

            Event::ClipboardStore(clipboard_type, data) => {
                self.send(TerminalEvent::ClipboardStore(clipboard_type, data))
            }
            Event::ClipboardLoad(clipboard_type, formatter) => {
                self.send(TerminalEvent::ClipboardLoad(clipboard_type, formatter))
            }

            // Answered inline: the requesting program is blocked on stdin.
            Event::PtyWrite(text) => self.with_notifier(|notifier| notifier.write_str(&text)),
            Event::ColorRequest(index, formatter) => self.with_notifier(|notifier| {
                let rgb = notifier.color(index);
                notifier.write_str(&formatter(rgb));
            }),
            Event::TextAreaSizeRequest(formatter) => self.with_notifier(|notifier| {
                let size = notifier.window_size();
                notifier.write_str(&formatter(size));
            }),

            Event::CursorBlinkingChange => self.send(TerminalEvent::CursorBlinkingChange),
            Event::MouseCursorDirty => self.send(TerminalEvent::MouseCursorDirty),

            Event::ChildExit(exit_code) => self.send(TerminalEvent::ChildExit(exit_code)),
            Event::Exit => self.send(TerminalEvent::Exit),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn notifier(sink: PtyWriter) -> TerminalNotifier {
        TerminalNotifier::new(
            sink,
            Arc::new(Mutex::new(WindowSize {
                num_lines: 24,
                num_cols: 80,
                cell_width: 8,
                cell_height: 16,
            })),
            Arc::new(Mutex::new(ColorPalette::default())),
        )
    }

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_event_proxy_creation() {
        let (tx, _rx) = channel();
        let _proxy = GpuiEventProxy::new(tx);
    }

    #[test]
    fn test_wakeup_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        proxy.send_event(Event::Wakeup);
        assert!(matches!(rx.recv().unwrap(), TerminalEvent::Wakeup));
    }

    #[test]
    fn test_bell_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        proxy.send_event(Event::Bell);
        assert!(matches!(rx.recv().unwrap(), TerminalEvent::Bell));
    }

    #[test]
    fn test_title_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        proxy.send_event(Event::Title("Test Title".to_string()));
        match rx.recv().unwrap() {
            TerminalEvent::Title(title) => assert_eq!(title, "Test Title"),
            other => panic!("Expected Title event, got {other:?}"),
        }
    }

    #[test]
    fn test_clipboard_store_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        proxy.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "clipboard data".to_string(),
        ));
        match rx.recv().unwrap() {
            TerminalEvent::ClipboardStore(_, data) => assert_eq!(data, "clipboard data"),
            other => panic!("Expected ClipboardStore event, got {other:?}"),
        }
    }

    #[test]
    fn test_clipboard_load_event_is_forwarded_with_formatter() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        let callback: ClipboardFormatter = Arc::new(|s: &str| format!("<{s}>"));
        proxy.send_event(Event::ClipboardLoad(ClipboardType::Clipboard, callback));

        match rx.recv().unwrap() {
            TerminalEvent::ClipboardLoad(_, formatter) => assert_eq!(formatter("hi"), "<hi>"),
            other => panic!("Expected ClipboardLoad event, got {other:?}"),
        }
    }

    #[test]
    fn test_pty_write_is_answered_inline() {
        let (tx, _rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        let recorder = Recorder::default();
        let sink: PtyWriter = Arc::new(Mutex::new(Box::new(recorder.clone())));
        *proxy.notifier_slot().lock() = Some(notifier(sink));

        proxy.send_event(Event::PtyWrite("\x1b[1;1R".to_string()));
        assert_eq!(&*recorder.0.lock(), b"\x1b[1;1R");
    }

    #[test]
    fn test_text_area_size_request_is_answered_inline() {
        let (tx, _rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        let recorder = Recorder::default();
        let sink: PtyWriter = Arc::new(Mutex::new(Box::new(recorder.clone())));
        *proxy.notifier_slot().lock() = Some(notifier(sink));

        proxy.send_event(Event::TextAreaSizeRequest(Arc::new(|size: WindowSize| {
            format!("\x1b[4;{};{}t", size.num_lines, size.num_cols)
        })));
        assert_eq!(&*recorder.0.lock(), b"\x1b[4;24;80t");
    }

    #[test]
    fn test_color_request_is_answered_inline() {
        let (tx, _rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        let recorder = Recorder::default();
        let sink: PtyWriter = Arc::new(Mutex::new(Box::new(recorder.clone())));
        *proxy.notifier_slot().lock() = Some(notifier(sink));

        proxy.send_event(Event::ColorRequest(
            0,
            Arc::new(|rgb: Rgb| format!("{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b)),
        ));
        assert_eq!(recorder.0.lock().len(), 6);
    }

    #[test]
    fn test_child_exit_reports_status() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        proxy.send_event(Event::ChildExit(3));
        match rx.recv().unwrap() {
            TerminalEvent::ChildExit(code) => assert_eq!(code, 3),
            other => panic!("Expected ChildExit, got {other:?}"),
        }
    }

    #[test]
    fn test_reset_title_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        proxy.send_event(Event::ResetTitle);
        assert!(matches!(rx.recv().unwrap(), TerminalEvent::ResetTitle));
    }

    #[test]
    fn test_disconnected_channel_is_not_fatal() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        drop(rx);
        proxy.send_event(Event::Wakeup);
    }
}
