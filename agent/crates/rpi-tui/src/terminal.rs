//! Terminal abstraction for TUI.
//!
//! Provides a trait for terminal operations and a `ProcessTerminal` implementation
//! using crossterm.

use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyEvent, MouseEvent},
    terminal as cterm,
};
use thiserror::Error;

/// Mouse tracking prevents native text selection in macOS terminal emulators.
/// Preserve that platform default; elsewhere the TUI handles selection and scrolling.
const fn mouse_tracking_enabled_for(is_macos: bool) -> bool {
    !is_macos
}

const fn mouse_tracking_enabled_by_default() -> bool {
    mouse_tracking_enabled_for(cfg!(target_os = "macos"))
}

/// Terminals that advertise keyboard-enhancement support but stop delivering
/// IME composition once the protocol is pushed (no candidate window → CJK input
/// breaks). Otty is the reported case; it keeps the legacy input path.
#[cfg(any(unix, test))]
fn is_ime_incompatible_terminal(term_program: Option<&str>, term: Option<&str>) -> bool {
    let names_otty = |value: Option<&str>| {
        value.is_some_and(|value| value.to_ascii_lowercase().contains("otty"))
    };
    names_otty(term_program) || names_otty(term)
}

/// The kitty keyboard protocol (`\x1b[>7u`) is what tells `Shift+Enter` apart
/// from `Enter` in most terminals, and it carries the key-release events
/// push-to-talk extensions need — so keep it on everywhere the terminal
/// survives it, macOS included. Only a terminal whose IME composition it breaks
/// falls back to legacy input; the one emulator that cannot opt in at all
/// (Apple Terminal) is covered by [`normalize_key_event`]'s native Shift check.
#[cfg(any(unix, test))]
fn keyboard_protocol_enabled_for(
    is_macos: bool,
    term_program: Option<&str>,
    term: Option<&str>,
) -> bool {
    !is_macos || !is_ime_incompatible_terminal(term_program, term)
}

/// `RPI_KITTY_KEYBOARD=1` forces the protocol on, `RPI_KITTY_KEYBOARD=0` forces
/// legacy input; otherwise [`keyboard_protocol_enabled_for`] decides from the
/// platform and the terminal name.
#[cfg(unix)]
fn keyboard_protocol_enabled() -> bool {
    if let Some(value) = std::env::var_os("RPI_KITTY_KEYBOARD") {
        return value != "0";
    }
    keyboard_protocol_enabled_for(
        cfg!(target_os = "macos"),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
    )
}

/// Errors that can occur during terminal operations.
#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Terminal error: {0}")]
    Terminal(String),
}

/// Terminal capabilities and state.
#[derive(Debug, Clone, Default)]
pub struct TerminalInfo {
    pub columns: usize,
    pub rows: usize,
    pub supports_true_color: bool,
    pub supports_mouse: bool,
    pub supports_kitty_keyboard: bool,
}

/// Input event types.
#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(usize, usize),
    FocusGained,
    FocusLost,
}

/// Terminal trait for abstracting terminal operations.
///
/// This allows for different terminal implementations (real terminal, virtual terminal for testing).
pub trait Terminal: Send + Sync {
    /// Get terminal information (columns, rows, capabilities).
    fn info(&self) -> TerminalInfo;

    /// Get current terminal width in columns.
    fn columns(&self) -> usize {
        self.info().columns
    }

    /// Get current terminal height in rows.
    fn rows(&self) -> usize {
        self.info().rows
    }

    /// Write raw bytes to the terminal.
    fn write(&self, data: &str);

    /// Hide the cursor.
    fn hide_cursor(&self);

    /// Show the cursor.
    fn show_cursor(&self);

    /// Move cursor to position (0-indexed).
    fn move_cursor(&self, row: usize, col: usize);

    /// Clear the screen.
    fn clear_screen(&self);

    /// Set terminal title.
    fn set_title(&self, title: &str);

    /// Enable mouse events.
    fn enable_mouse(&self);

    /// Disable mouse events.
    fn disable_mouse(&self);

    /// Enter terminal raw mode (required for `event::read()` to deliver
    /// individual key events without line buffering). Pairs with
    /// [`Terminal::stop`], which exits raw mode.
    ///
    /// Callers that drive their own input loop (instead of the reader thread
    /// spawned by [`Terminal::start`]) use this to set up the tty without
    /// spawning a competing reader — see `TuiAltScreen::start_readerless`.
    fn enter_raw_mode(&self);

    /// Leave raw mode *without* stopping the terminal, so the shell gets a
    /// working line discipline back. Pairs with [`Terminal::enter_raw_mode`];
    /// a `Ctrl+Z` suspend uses it, then re-enters raw mode on resume.
    fn exit_raw_mode(&self);

    /// Re-read the current terminal size into the cached [`TerminalInfo`].
    ///
    /// Called by the renderer before laying out every frame and by the hosts'
    /// size watchdog, so a window that changed without a usable resize event
    /// still gets a correct frame.
    fn refresh_size(&self);

    /// The terminal's pixel size for its current character grid, when the
    /// platform can report one.
    ///
    /// Image rows are derived from the size of a cell in pixels, and the
    /// character grid can stay identical while that changes (font zoom, a move
    /// to a monitor with another DPI). Asking the terminal object rather than a
    /// process-wide cache keeps the answer attached to the terminal it came
    /// from, and gives tests an injection point that needs no global state.
    ///
    /// Defaults to `None`: a terminal that cannot report pixels (Windows, or a
    /// Unix terminal that leaves the ioctl fields at zero) leaves image sizing
    /// at the previously known geometry.
    fn window_size(&self) -> Option<crossterm::terminal::WindowSize> {
        None
    }

    /// Start terminal raw mode.
    fn start(
        &self,
        on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
        on_resize: Box<dyn Fn() + Send + Sync>,
    );

    /// Stop terminal and restore original state.
    fn stop(&self);

    /// Check if terminal is a TTY.
    fn is_tty(&self) -> bool;

    /// Set progress indicator (terminal progress bar support).
    fn set_progress(&self, active: bool);

    /// Flush output buffer.
    fn flush(&self);
}

/// Process-based terminal using crossterm.
pub struct ProcessTerminal {
    info: Mutex<TerminalInfo>,
    running: Mutex<bool>,
    writer: Mutex<Box<dyn Write + Send + Sync>>,
    #[cfg(windows)]
    mouse_captured: Mutex<bool>,
    #[cfg(any(unix, test))]
    keyboard: Mutex<KeyboardProtocolState>,
}

#[cfg(any(unix, test))]
#[derive(Clone, Copy)]
enum KeyboardProtocol {
    Kitty,
    Legacy,
}

#[cfg(any(unix, test))]
#[derive(Default)]
struct KeyboardProtocolState {
    detected: Option<bool>,
    active: Option<KeyboardProtocol>,
}

impl ProcessTerminal {
    /// Create a new process terminal.
    pub fn new() -> Self {
        let (cols, rows) = cterm::size().unwrap_or((80, 24));
        let info = TerminalInfo {
            columns: cols as usize,
            rows: rows as usize,
            supports_true_color: true, // Assume true color support
            supports_mouse: true,
            supports_kitty_keyboard: false, // Will be detected
        };

        Self {
            info: Mutex::new(info),
            running: Mutex::new(false),
            writer: Mutex::new(Box::new(io::stdout())),
            #[cfg(windows)]
            mouse_captured: Mutex::new(false),
            #[cfg(any(unix, test))]
            keyboard: Mutex::new(KeyboardProtocolState::default()),
        }
    }

    /// Update terminal size.
    fn update_size(&self) {
        if let Ok(size) = cterm::size() {
            if let Ok(mut info) = self.info.lock() {
                info.columns = size.0 as usize;
                info.rows = size.1 as usize;
            }
        }
    }

    fn configure_mouse_tracking(&self) {
        if mouse_tracking_enabled_by_default() {
            self.enable_mouse();
        } else {
            self.disable_mouse();
        }
    }

    /// Negotiate before starting the input reader. Cache support so resuming
    /// never queries stdin while a reader is already running.
    #[cfg(any(unix, test))]
    fn enable_keyboard_protocol(&self, detect: impl FnOnce() -> bool) {
        let mut keyboard = self.keyboard.lock().unwrap_or_else(|p| p.into_inner());
        if keyboard.active.is_some() {
            return;
        }
        let supported = match keyboard.detected {
            Some(supported) => supported,
            None => {
                // Match Pi: 1: disambiguate, 2: repeat/release, 4: alternate keys.
                // Leave text as UTF-8 for IME commits. Flag 8 replaces text
                // with key reports; crossterm cannot consume associated text.
                self.write("\x1b[>7u");
                self.flush();
                let supported = detect();
                keyboard.detected = Some(supported);
                if !supported {
                    self.write("\x1b[<u");
                }
                if let Ok(mut info) = self.info.lock() {
                    info.supports_kitty_keyboard = supported;
                }
                if supported {
                    keyboard.active = Some(KeyboardProtocol::Kitty);
                    return;
                }
                false
            }
        };
        let protocol = if supported {
            self.write("\x1b[>7u");
            KeyboardProtocol::Kitty
        } else {
            // Crossterm cannot parse xterm's CSI 27;modifier;key~ encoding.
            // Keep legacy input rather than enabling modifyOtherKeys and
            // losing keys. Apple Terminal's Shift+Enter is normalized below.
            KeyboardProtocol::Legacy
        };
        keyboard.active = Some(protocol);
    }

    #[cfg(any(unix, test))]
    fn disable_keyboard_protocol(&self) {
        let mut keyboard = self.keyboard.lock().unwrap_or_else(|p| p.into_inner());
        match keyboard.active.take() {
            Some(KeyboardProtocol::Kitty) => self.write("\x1b[<u"),
            Some(KeyboardProtocol::Legacy) | None => {}
        }
    }
}

impl Default for ProcessTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Terminal for ProcessTerminal {
    fn info(&self) -> TerminalInfo {
        self.info.lock().map(|i| i.clone()).unwrap_or_default()
    }

    fn write(&self, data: &str) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(data.as_bytes());
        }
    }

    fn hide_cursor(&self) {
        self.write("\x1b[?25l");
    }

    fn show_cursor(&self) {
        self.write("\x1b[?25h");
    }

    fn move_cursor(&self, row: usize, col: usize) {
        // ANSI escape: ESC [ row ; col H (1-indexed)
        self.write(&format!("\x1b[{};{}H", row + 1, col + 1));
    }

    fn clear_screen(&self) {
        self.write("\x1b[2J\x1b[H");
    }

    fn set_title(&self, title: &str) {
        self.write(&format!("\x1b]2;{}\x07", title));
    }

    fn enable_mouse(&self) {
        #[cfg(windows)]
        {
            let mut captured = self
                .mouse_captured
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !*captured {
                let mut writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
                match crossterm::execute!(&mut *writer, event::EnableMouseCapture) {
                    Ok(()) => *captured = true,
                    Err(error) => tracing::warn!(%error, "Failed to enable mouse capture"),
                }
            }
        }
        #[cfg(not(windows))]
        // Enable mouse tracking (SGR mode)
        self.write("\x1b[?1000h\x1b[?1002h\x1b[?1006h");
    }

    fn disable_mouse(&self) {
        #[cfg(windows)]
        {
            let mut captured = self
                .mouse_captured
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if *captured {
                // Crossterm caches the entire input mode on the FIRST capture,
                // even across later activations. Preserve the current raw-mode
                // state rather than resurrecting that potentially stale state.
                let raw = cterm::is_raw_mode_enabled();
                let mut writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
                match crossterm::execute!(&mut *writer, event::DisableMouseCapture) {
                    Ok(()) => {
                        *captured = false;
                        let restored = match raw {
                            Ok(true) => cterm::enable_raw_mode(),
                            Ok(false) => cterm::disable_raw_mode(),
                            Err(error) => Err(error),
                        };
                        if let Err(error) = restored {
                            tracing::warn!(%error, "Failed to preserve raw mode after mouse capture");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "Failed to disable mouse capture"),
                }
            }
        }
        #[cfg(not(windows))]
        self.write("\x1b[?1006l\x1b[?1002l\x1b[?1000l");
    }

    fn enter_raw_mode(&self) {
        if let Err(error) = cterm::enable_raw_mode() {
            tracing::warn!(%error, "Failed to enter terminal raw mode");
        }
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }
        self.configure_mouse_tracking();
        self.hide_cursor();
        // Request bracketed paste. Unix crossterm decodes it as Event::Paste;
        // the Windows console backend still needs its native paste collector
        // because console translation can strip the bracket markers.
        self.write("\x1b[?2004h");
        #[cfg(unix)]
        if keyboard_protocol_enabled() && self.is_tty() && io::stdin().is_terminal() {
            self.enable_keyboard_protocol(|| {
                cterm::supports_keyboard_enhancement().unwrap_or(false)
            });
        }
        self.update_size();
        self.flush();
    }

    fn exit_raw_mode(&self) {
        // Mirror `enter_raw_mode` without tearing the terminal down: drop the
        // bracketed-paste mode it turned on and restore the line discipline.
        self.write("\x1b[?2004l");
        #[cfg(unix)]
        self.disable_keyboard_protocol();
        #[cfg(windows)]
        self.disable_mouse();
        self.flush();
        // Mouse capture may restore a cached raw input mode. Always restore
        // the shell's line discipline AFTER disabling capture.
        if let Err(error) = cterm::disable_raw_mode() {
            tracing::warn!(%error, "Failed to exit terminal raw mode");
        }
    }

    fn refresh_size(&self) {
        self.update_size();
    }

    fn window_size(&self) -> Option<crossterm::terminal::WindowSize> {
        cterm::window_size().ok()
    }

    fn start(
        &self,
        on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
        on_resize: Box<dyn Fn() + Send + Sync>,
    ) {
        self.enter_raw_mode();

        // Update size initially
        self.update_size();

        // Spawn input handling thread
        let running = Arc::new(Mutex::new(true));
        let running_clone = running.clone();
        let info_clone = Arc::new(Mutex::new(self.info()));
        let supports_kitty_keyboard = self.info().supports_kitty_keyboard;

        std::thread::spawn(move || {
            loop {
                // Check if we're still running
                if !*running_clone.lock().unwrap() {
                    break;
                }

                // Poll for events with timeout
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => {
                        if let Ok(event) = event::read() {
                            match event {
                                Event::Key(key) => on_input(InputEvent::Key(
                                    // Sample platform state as soon as the
                                    // event is read; see `normalize_key_event`.
                                    normalize_key_event_with_protocol(
                                        normalize_key_event(key),
                                        supports_kitty_keyboard,
                                    ),
                                )),
                                Event::Mouse(mouse) => on_input(InputEvent::Mouse(mouse)),
                                Event::Resize(cols, rows) => {
                                    if let Ok(mut info) = info_clone.lock() {
                                        info.columns = cols as usize;
                                        info.rows = rows as usize;
                                    }
                                    on_resize();
                                }
                                Event::FocusGained => on_input(InputEvent::FocusGained),
                                Event::FocusLost => on_input(InputEvent::FocusLost),
                                Event::Paste(_) => { /* Ignore paste events for now */ }
                            }
                        }
                    }
                    Ok(false) => {}  // Timeout, continue polling
                    Err(_) => break, // Error, stop the thread
                }
            }
        });

        // Store running flag
        if let Ok(mut r) = self.running.lock() {
            *r = true;
        }
    }

    fn stop(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }

        self.disable_mouse();
        self.show_cursor();
        // Disable bracketed paste mode before exiting raw mode.
        self.write("\x1b[?2004l");
        #[cfg(unix)]
        self.disable_keyboard_protocol();
        self.flush();

        // Exit raw mode only after mouse capture has restored its input mode.
        if let Err(error) = cterm::disable_raw_mode() {
            tracing::warn!(%error, "Failed to exit terminal raw mode");
        }
    }

    fn is_tty(&self) -> bool {
        std::io::stdout().is_terminal()
    }

    fn set_progress(&self, active: bool) {
        // OSC 9;4 is supported by modern terminal emulators (including
        // Windows Terminal, iTerm2-compatible terminals, and many Linux
        // terminals). State 1 starts an indeterminate progress indicator and
        // state 0 clears it.
        let state = if active { 1 } else { 0 };
        self.write(&format!("\x1b]9;4;{state};0\x07"));
        self.flush();
    }

    fn flush(&self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.flush();
        }
    }
}

/// Apple Terminal and the Windows console both send the same bare CR for Enter
/// and Shift+Enter. Like Pi's native helper, consult the local modifier state
/// only for that legacy key, and only for a local session: SSH input belongs to
/// the remote user, not to this machine's physical keyboard.
///
/// Call this once, as soon as an event is decoded — `rpi-cli`'s
/// `normalize_decoded_event` does, on the way out of `crossterm::event::read`.
/// The sample describes the keyboard at call time, so it is only as good as how
/// close the call is to the keypress; repeating it later, after routing or
/// rendering, can attribute a *later* Shift press to an older Enter.
///
/// Event-time data wins wherever the transport still carries it. crossterm maps
/// `KEY_EVENT_RECORD::dwControlKeyState` onto `KeyModifiers` for every key,
/// Enter included, so a console record that says `SHIFT_PRESSED` arrives
/// already modified and neither branch below touches it. The Windows branch
/// covers only the case where that bit never reached the record: legacy VT
/// input through ConPTY encodes both chords as `\r`
/// (microsoft/terminal#4999), which is what `RPI_DEBUG_KEYS=1` shows as
/// `code=Enter mods=NONE kind=Press` for either key.
///
/// That fallback is best effort, not a reconstruction of the original chord.
/// The information is gone by the time the record exists; only
/// `win32-input-mode` (`CSI ? 9001 h`) would put it back, and it leaves the
/// user's shell unusable if rpi dies without resetting it
/// (microsoft/terminal#19576, fish-shell#11877), so it is deliberately not
/// enabled for one keybinding. Consequences, accepted deliberately:
///
/// * Shift+Enter sampled after the user already released Shift submits, which
///   is what it did before the fallback existed.
/// * Plain Enter sampled after Shift went down for the *next* keystroke inserts
///   a newline instead of submitting. Nothing is lost: the next Enter with
///   Shift up submits the draft.
pub fn normalize_key_event(key: KeyEvent) -> KeyEvent {
    #[cfg(target_os = "macos")]
    if key.code == event::KeyCode::Enter
        && key.modifiers.is_empty()
        && local_apple_terminal(std::env::var("TERM_PROGRAM").ok().as_deref(), ssh_session())
    {
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGEventSourceFlagsState(state_id: i32) -> u64;
        }
        // kCGEventSourceStateCombinedSessionState = 0; maskShift = 1 << 17.
        // This accessor reads modifier flags without creating an event tap.
        let shift_pressed = unsafe { CGEventSourceFlagsState(0) } & (1 << 17) != 0;
        return normalize_native_shift_enter(key, shift_pressed);
    }
    #[cfg(windows)]
    if key.code == event::KeyCode::Enter && key.modifiers.is_empty() {
        return normalize_windows_key_event(key, shift_key_pressed(), ssh_session());
    }
    key
}

/// Normalize decoded input using the protocol negotiated by this terminal.
/// Legacy iTerm2 uses CSI 1;9 A/B/C/D for Option+arrows; crossterm decodes
/// modifier 9 as Super (the kitty meaning). Keep actual kitty Super keys intact.
///
/// Platform sampling stays in [`normalize_key_event`], which callers run once
/// when the event is read; it is not repeated here. A second sample at dispatch
/// time could turn a plain Enter into a newline because Shift went down for a
/// later keystroke while the event waited in the queue.
pub fn normalize_key_event_with_protocol(key: KeyEvent, supports_kitty_keyboard: bool) -> KeyEvent {
    normalize_legacy_iterm_arrow(
        key,
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        supports_kitty_keyboard,
    )
}

fn normalize_legacy_iterm_arrow(
    mut key: KeyEvent,
    term_program: Option<&str>,
    supports_kitty_keyboard: bool,
) -> KeyEvent {
    use event::{KeyCode, KeyModifiers};
    if term_program == Some("iTerm.app")
        && !supports_kitty_keyboard
        && matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
        )
        && key.modifiers.contains(KeyModifiers::SUPER)
    {
        key.modifiers.remove(KeyModifiers::SUPER);
        key.modifiers.insert(KeyModifiers::ALT);
    }
    key
}

/// SSH sessions deliver a remote user's keys; a physical-keyboard check on this
/// machine must never rewrite them. Same gate for both platform fallbacks.
#[cfg(any(target_os = "macos", windows))]
fn ssh_session() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

#[cfg(any(target_os = "macos", test))]
fn local_apple_terminal(term_program: Option<&str>, ssh: bool) -> bool {
    term_program == Some("Apple_Terminal") && !ssh
}

/// `true` when rpi runs on this machine's console rather than serving a remote
/// user. A Shift check makes sense only against the local physical keyboard;
/// remote input must stay untouched, mirroring the macOS gate above.
#[cfg(any(windows, test))]
fn local_windows_session(ssh: bool) -> bool {
    !ssh
}

/// Restore `SHIFT` on a bare Enter from the Windows console, or leave the event
/// alone for a remote session. Split out from [`normalize_key_event`] so the
/// decision is testable off Windows; `shift_down` is the physical state sampled
/// by [`shift_key_pressed`] when the event was read.
#[cfg(any(windows, test))]
fn normalize_windows_key_event(key: KeyEvent, shift_down: bool, ssh: bool) -> KeyEvent {
    if local_windows_session(ssh) {
        normalize_native_shift_enter(key, shift_down)
    } else {
        key
    }
}

/// Read the physical Shift state alongside the console's input thread.
///
/// crossterm's Windows reader delivers each key as a console `INPUT_RECORD`,
/// and for the Enter key pair the modifier never survives the terminal: both
/// Enter and Shift+Enter arrive as a bare CR. `GetAsyncKeyState` reports the
/// physical key state at call time — the Windows analog of the macOS
/// `CGEventSourceFlagsState` read above, and the reason [`normalize_key_event`]
/// is called as soon as the event is read rather than at dispatch.
/// `GetKeyState` is not usable here: its per-thread state only updates as a
/// thread reads window messages, and a console input thread pumps none, so it
/// is unreliable in console apps.
#[cfg(windows)]
fn shift_key_pressed() -> bool {
    use winapi::um::winuser::{GetAsyncKeyState, VK_SHIFT};
    // The high bit is set while the physical key is down; the low bit ("pressed
    // since the last call") is documented as unreliable and is ignored.
    (unsafe { GetAsyncKeyState(VK_SHIFT as i32) } as u16) & 0x8000 != 0
}

#[cfg(any(target_os = "macos", windows, test))]
fn normalize_native_shift_enter(mut key: KeyEvent, shift_pressed: bool) -> KeyEvent {
    if key.code == event::KeyCode::Enter && key.modifiers.is_empty() && shift_pressed {
        key.modifiers.insert(event::KeyModifiers::SHIFT);
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_iterm_option_up_matches_dequeue_without_changing_kitty_super() {
        use event::{KeyCode, KeyEventKind, KeyModifiers};
        let mut bindings = crate::Keybindings::new();
        bindings.set(
            crate::keybindings::keys::DEQUEUE,
            vec![crate::KeyCombo::new(KeyCode::Up, KeyModifiers::ALT)],
        );
        // CSI 1;9A decodes as Up+Super in crossterm 0.27.
        let legacy =
            KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::SUPER, KeyEventKind::Repeat);
        let normalized = normalize_legacy_iterm_arrow(legacy, Some("iTerm.app"), false);
        assert!(bindings.matches(&normalized, crate::keybindings::keys::DEQUEUE));
        assert_eq!(normalized.kind, KeyEventKind::Repeat);
        assert_eq!(
            normalize_legacy_iterm_arrow(legacy, Some("iTerm.app"), true),
            legacy
        );
        assert_eq!(
            normalize_legacy_iterm_arrow(legacy, Some("ghostty"), false),
            legacy
        );
        assert_eq!(normalize_legacy_iterm_arrow(legacy, None, false), legacy);
        let plain = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            normalize_legacy_iterm_arrow(plain, Some("iTerm.app"), false),
            plain
        );
        let alt = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(
            normalize_legacy_iterm_arrow(alt, Some("iTerm.app"), false),
            alt
        );
        let letter = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SUPER);
        assert_eq!(
            normalize_legacy_iterm_arrow(letter, Some("iTerm.app"), false),
            letter
        );
        let shifted = KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER | KeyModifiers::SHIFT);
        assert_eq!(
            normalize_legacy_iterm_arrow(shifted, Some("iTerm.app"), false).modifiers,
            KeyModifiers::ALT | KeyModifiers::SHIFT
        );
    }

    #[derive(Clone)]
    struct RecordedOutput(Arc<Mutex<Vec<u8>>>);
    impl Write for RecordedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn kitty_protocol_preserves_ime_text_and_restores_once_per_activation() {
        let terminal = ProcessTerminal::new();
        let output = RecordedOutput(Arc::new(Mutex::new(Vec::new())));
        *terminal.writer.lock().unwrap() = Box::new(output.clone());
        terminal.enable_keyboard_protocol(|| true);
        assert!(terminal.info().supports_kitty_keyboard);
        terminal.enable_keyboard_protocol(|| panic!("active protocol must not be pushed twice"));
        terminal.disable_keyboard_protocol();
        terminal.disable_keyboard_protocol();
        terminal.enable_keyboard_protocol(|| panic!("resume must not query stdin"));
        terminal.disable_keyboard_protocol();
        let bytes = output.0.lock().unwrap().clone();
        assert_eq!(bytes, b"\x1b[>7u\x1b[<u\x1b[>7u\x1b[<u");
        // Check the actual emitted flags to prevent reintroducing all-key mode.
        let sequence = std::str::from_utf8(&bytes)
            .unwrap()
            .split('u')
            .next()
            .unwrap();
        let mask: u8 = sequence.strip_prefix("\x1b[>").unwrap().parse().unwrap();
        let flags = event::KeyboardEnhancementFlags::from_bits(mask).unwrap();
        assert!(flags.contains(event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES));
        assert!(flags.contains(event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        assert!(flags.contains(event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
        assert!(!flags.contains(event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
        // Bit 16 is associated-text reporting, unsupported by crossterm 0.27.
        assert_eq!(mask & 16, 0);
    }

    #[test]
    fn legacy_terminal_does_not_enable_an_encoding_the_reader_cannot_parse() {
        let terminal = ProcessTerminal::new();
        let output = RecordedOutput(Arc::new(Mutex::new(Vec::new())));
        *terminal.writer.lock().unwrap() = Box::new(output.clone());
        terminal.enable_keyboard_protocol(|| false);
        assert!(!terminal.info().supports_kitty_keyboard);
        terminal.disable_keyboard_protocol();
        terminal.enable_keyboard_protocol(|| panic!("resume must reuse detection"));
        terminal.disable_keyboard_protocol();
        assert_eq!(output.0.lock().unwrap().as_slice(), b"\x1b[>7u\x1b[<u");
    }

    #[test]
    fn native_shift_enter_only_changes_unmodified_enter_in_local_apple_terminal() {
        use event::{KeyCode, KeyModifiers};
        assert!(local_apple_terminal(Some("Apple_Terminal"), false));
        assert!(!local_apple_terminal(Some("Apple_Terminal"), true));
        assert!(!local_apple_terminal(Some("iTerm.app"), false));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            normalize_native_shift_enter(enter, true).modifiers,
            KeyModifiers::SHIFT
        );
        assert_eq!(normalize_native_shift_enter(enter, false), enter);
        for key in [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        ] {
            assert_eq!(normalize_native_shift_enter(key, true), key);
        }
    }

    #[test]
    fn windows_native_shift_enter_only_touches_unmodified_enter_of_a_local_session() {
        use event::{KeyCode, KeyModifiers};
        assert!(local_windows_session(false));
        assert!(!local_windows_session(true));
        // `shift_down` stands in for the GetAsyncKeyState sample taken when the
        // event is read; the real call needs a console session and is covered
        // by the real-machine matrix instead.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            normalize_windows_key_event(enter, true, false).modifiers,
            KeyModifiers::SHIFT
        );
        assert_eq!(normalize_windows_key_event(enter, false, false), enter);
        // A remote session never consults this machine's keyboard.
        assert_eq!(normalize_windows_key_event(enter, true, true), enter);
        // The console record stays authoritative: whatever it already carries
        // is left alone, so a transport that preserves SHIFT_PRESSED needs no
        // fallback at all.
        for key in [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        ] {
            assert_eq!(normalize_windows_key_event(key, true, false), key);
        }
    }

    /// The physical sample is taken once, when the event is read, and the
    /// dispatch-time protocol pass must not sample again: a Shift press that
    /// lands after an Enter was read belongs to the next keystroke, not to that
    /// Enter.
    #[test]
    fn windows_native_shift_enter_is_decided_once_when_the_event_is_read() {
        use event::{KeyCode, KeyModifiers};
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        // Read while Shift is down: the chord reaches the editor intact.
        let chord = normalize_key_event_with_protocol(
            normalize_windows_key_event(enter, true, false),
            false,
        );
        assert_eq!(chord.modifiers, KeyModifiers::SHIFT);

        // Read with Shift up, Shift pressed for a later key before dispatch:
        // still a plain Enter. A second sample in the protocol pass would turn
        // this into a newline; on Linux the pass never samples, so this is a
        // Windows-build regression guard rather than a Linux assertion.
        let plain = normalize_key_event_with_protocol(
            normalize_windows_key_event(enter, false, false),
            false,
        );
        assert_eq!(plain, enter);
    }

    #[test]
    fn native_shift_enter_inserts_a_line_before_plain_enter_submits() {
        use event::{KeyCode, KeyModifiers};
        let editor = crate::Editor::simple();
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let capture = submitted.clone();
        editor.on_submit(Arc::new(move |text| {
            capture.lock().unwrap().push(text.to_string())
        }));
        editor.insert("first");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        editor.handle_key(normalize_native_shift_enter(enter, true));
        editor.insert("second");
        assert_eq!(editor.get_text(), "first\nsecond");
        assert!(submitted.lock().unwrap().is_empty());
        editor.handle_key(normalize_native_shift_enter(enter, false));
        assert_eq!(submitted.lock().unwrap().as_slice(), &["first\nsecond"]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_mouse_capture_restores_console_across_repeated_raw_mode_cycles() {
        use std::os::windows::io::AsRawHandle;
        use winapi::um::{
            consoleapi::{GetConsoleMode, SetConsoleMode},
            wincon::{ENABLE_EXTENDED_FLAGS, ENABLE_MOUSE_INPUT, ENABLE_QUICK_EDIT_MODE},
        };

        // A redirected test runner may still have CONIN$; test the same real
        // console crossterm uses, not stdout's TTY status. Never allocate one.
        let console = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONIN$")
        {
            Ok(console) => console,
            Err(error) => {
                eprintln!("SKIP windows mouse capture: no real console ({error})");
                return;
            }
        };
        fn mode(console: &std::fs::File) -> io::Result<u32> {
            let mut mode = 0;
            if unsafe { GetConsoleMode(console.as_raw_handle().cast(), &mut mode) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(mode)
            }
        }
        let original = match mode(&console) {
            Ok(mode) => mode,
            Err(error) => {
                eprintln!("SKIP windows mouse capture: no console input mode ({error})");
                return;
            }
        };
        struct RestoreConsole(std::fs::File, u32);
        impl Drop for RestoreConsole {
            fn drop(&mut self) {
                if unsafe { SetConsoleMode(self.0.as_raw_handle().cast(), self.1) } == 0 {
                    eprintln!(
                        "Failed to restore test console: {}",
                        io::Error::last_os_error()
                    );
                }
            }
        }
        let restore = RestoreConsole(console, original);
        let terminal = ProcessTerminal::new();
        *terminal.writer.lock().unwrap() =
            Box::new(RecordedOutput(Arc::new(Mutex::new(Vec::new()))));
        let assert_capture = || {
            let mode = mode(&restore.0).unwrap();
            assert_ne!(mode & ENABLE_MOUSE_INPUT, 0);
            assert_ne!(mode & ENABLE_EXTENDED_FLAGS, 0);
            assert_eq!(mode & ENABLE_QUICK_EDIT_MODE, 0);
            assert!(cterm::is_raw_mode_enabled().unwrap());
            assert!(*terminal.mouse_captured.lock().unwrap());
        };

        // Disabling before enabling must not ask crossterm to restore an
        // uninitialized cache or modify the console at all.
        terminal.disable_mouse();
        assert_eq!(mode(&restore.0).unwrap(), original);
        for _ in 0..2 {
            terminal.enter_raw_mode();
            terminal.enter_raw_mode();
            terminal.enable_mouse();
            assert_capture();
            terminal.disable_mouse();
            terminal.disable_mouse();
            assert!(cterm::is_raw_mode_enabled().unwrap());
            terminal.enable_mouse();
            assert_capture();
            terminal.exit_raw_mode();
            terminal.exit_raw_mode();
            assert!(!*terminal.mouse_captured.lock().unwrap());
            assert!(!cterm::is_raw_mode_enabled().unwrap());
            terminal.stop();
            terminal.stop();
            assert!(!cterm::is_raw_mode_enabled().unwrap());
        }
        terminal.enter_raw_mode();
        assert_capture();
        terminal.stop();
        terminal.stop();
        assert!(!*terminal.mouse_captured.lock().unwrap());
        assert!(!cterm::is_raw_mode_enabled().unwrap());

        // A capture started outside raw mode must not make a later explicit
        // disable_mouse unexpectedly leave raw mode.
        terminal.enable_mouse();
        terminal.enter_raw_mode();
        terminal.disable_mouse();
        assert!(cterm::is_raw_mode_enabled().unwrap());
        terminal.stop();
        assert!(!cterm::is_raw_mode_enabled().unwrap());
    }

    #[test]
    fn mouse_tracking_default_preserves_native_selection_on_macos() {
        assert!(!mouse_tracking_enabled_for(true));
        assert!(mouse_tracking_enabled_for(false));
    }

    #[test]
    fn macos_keeps_the_kitty_protocol_except_where_it_breaks_ime() {
        // iTerm2's Shift+Enter needs the protocol: disabling it macOS-wide in
        // 0.3.21 made Shift+Enter submit instead of inserting a newline.
        assert!(keyboard_protocol_enabled_for(
            true,
            Some("iTerm.app"),
            Some("xterm-256color")
        ));
        assert!(keyboard_protocol_enabled_for(true, Some("ghostty"), None));
        assert!(keyboard_protocol_enabled_for(
            true,
            Some("Apple_Terminal"),
            None
        ));
        // An unknown local terminal keeps the protocol rather than silently
        // losing Shift+Enter; `RPI_KITTY_KEYBOARD=0` is the escape hatch.
        assert!(keyboard_protocol_enabled_for(
            true,
            None,
            Some("xterm-256color")
        ));
        // Otty composes CJK input only on the legacy path.
        assert!(!keyboard_protocol_enabled_for(
            true,
            Some("Otty"),
            Some("xterm-256color")
        ));
        assert!(!keyboard_protocol_enabled_for(true, None, Some("otty")));
        // Other platforms never consult the terminal name.
        assert!(keyboard_protocol_enabled_for(
            false,
            Some("Otty"),
            Some("otty")
        ));
    }

    #[test]
    fn ime_committed_utf8_text_reaches_editor_without_submitting() {
        use event::{KeyCode, KeyModifiers};
        let editor = crate::Editor::simple();
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let capture = submitted.clone();
        editor.on_submit(Arc::new(move |text| {
            capture.lock().unwrap().push(text.to_string())
        }));
        let committed = "你好，世界𠮷 abc";
        for ch in committed.chars() {
            editor.handle_key(normalize_key_event(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(editor.get_text(), committed);
        assert!(submitted.lock().unwrap().is_empty());
        editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(submitted.lock().unwrap().as_slice(), &[committed]);
    }
}
