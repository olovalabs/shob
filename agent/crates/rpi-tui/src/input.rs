//! Input component - single-line text input with horizontal scrolling.
//!
//! Provides a text input field with cursor positioning, selection,
//! undo/redo, and Emacs-style editing.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::{Component, Focusable};
use super::keybindings::Keybindings;
use super::kill_ring::KillRing;
use super::undo_stack::UndoStack;
use super::word_navigation::{find_word_backward, find_word_forward};
use crate::ansi::CURSOR_MARKER;
use crate::utils::{slice_by_column, visible_width};

/// Byte offset of the `char_index`-th character, clamped to the end of the
/// string.
///
/// The cursor is a *character* index (matching `word_navigation`, which also
/// indexes by char) but `&str` can only be sliced at byte offsets, so every
/// splice has to go through this mapping. Indexing a `&str` directly by the
/// char index both panicked on non-ASCII input and desynced the caret.
fn byte_offset(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(offset, _)| offset)
        .unwrap_or(value.len())
}

/// Number of characters in `value`.
fn char_count(value: &str) -> usize {
    value.chars().count()
}

/// Input state for undo.
#[derive(Debug, Clone, Default)]
struct InputState {
    value: String,
    cursor: usize,
}

/// Last action type for kill ring coalescing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LastAction {
    None,
    Kill,
    Yank,
    TypeWord,
}

/// Input component - single-line text input.
pub struct Input {
    state: Mutex<InputState>,
    prompt: String,
    placeholder: Option<String>,
    masked: Mutex<bool>,
    #[allow(dead_code)]
    keybindings: Arc<Keybindings>,
    kill_ring: Mutex<KillRing>,
    undo_stack: Mutex<UndoStack<InputState>>,
    last_action: Mutex<LastAction>,
    focused: Mutex<bool>,
    on_submit: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    on_escape: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    on_change: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
}

impl Input {
    /// Create a new input component.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InputState::default()),
            prompt: "> ".to_string(),
            placeholder: None,
            masked: Mutex::new(false),
            keybindings: Arc::new(Keybindings::new()),
            kill_ring: Mutex::new(KillRing::new()),
            undo_stack: Mutex::new(UndoStack::new()),
            last_action: Mutex::new(LastAction::None),
            focused: Mutex::new(false),
            on_submit: Mutex::new(None),
            on_escape: Mutex::new(None),
            on_change: Mutex::new(None),
        }
    }

    /// Create an input with a custom prompt.
    pub fn with_prompt(prompt: &str) -> Self {
        let mut input = Self::new();
        input.prompt = prompt.to_string();
        input
    }

    /// Create an input with a placeholder.
    pub fn with_placeholder(placeholder: &str) -> Self {
        let mut input = Self::new();
        input.placeholder = Some(placeholder.to_string());
        input
    }

    /// Mask the entered value while retaining the real value for submission.
    pub fn set_masked(&self, masked: bool) {
        if let Ok(mut value) = self.masked.lock() {
            *value = masked;
        }
    }

    /// Get the current value.
    pub fn get_value(&self) -> String {
        self.state
            .lock()
            .map(|s| s.value.clone())
            .unwrap_or_default()
    }

    /// Set the value.
    pub fn set_value(&self, value: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.value = value.to_string();
            // Character index, not byte length: the cursor is char-counted.
            state.cursor = char_count(value);
        }
    }

    /// Clear the input.
    pub fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.value.clear();
            state.cursor = 0;
        }
    }

    /// Set callback for when input is submitted (Enter pressed).
    pub fn on_submit(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_submit.lock() {
            *cb = Some(callback);
        }
    }

    /// Set callback for when escape is pressed.
    pub fn on_escape(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut cb) = self.on_escape.lock() {
            *cb = Some(callback);
        }
    }

    /// Set callback for when value changes.
    pub fn on_change(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_change.lock() {
            *cb = Some(callback);
        }
    }

    /// Handle bracketed paste.
    pub fn handle_paste(&self, pasted_text: &str) {
        self.set_last_action(LastAction::None);

        // Clean the pasted text - remove newlines and tabs
        let clean_text = pasted_text
            .replace("\r\n", "")
            .replace("\r", "")
            .replace("\n", "")
            .replace("\t", "    ");

        self.edit(false, |state| {
            if clean_text.is_empty() {
                return false;
            }
            let at = byte_offset(&state.value, state.cursor);
            state.value.insert_str(at, &clean_text);
            state.cursor += clean_text.chars().count();
            true
        });
    }

    /// Insert a character at the cursor.
    fn insert_character(&self, char: &str) {
        // Undo coalescing: consecutive word chars coalesce into one undo unit
        let whitespace = char
            .chars()
            .next()
            .map(|c| c.is_whitespace())
            .unwrap_or(true);
        let coalesce = !whitespace && self.get_last_action() == LastAction::TypeWord;
        self.set_last_action(LastAction::TypeWord);

        self.edit(coalesce, |state| {
            if char.is_empty() {
                return false;
            }
            let at = byte_offset(&state.value, state.cursor);
            state.value.insert_str(at, char);
            state.cursor += char.chars().count();
            true
        });
    }

    /// Handle backspace (delete character before cursor).
    fn handle_backspace(&self) {
        self.set_last_action(LastAction::None);
        self.edit(false, |state| {
            if state.cursor == 0 {
                return false;
            }
            let start = byte_offset(&state.value, state.cursor - 1);
            let end = byte_offset(&state.value, state.cursor);
            state.value.replace_range(start..end, "");
            state.cursor -= 1;
            true
        });
    }

    /// Handle forward delete (delete character after cursor).
    fn handle_forward_delete(&self) {
        self.set_last_action(LastAction::None);
        self.edit(false, |state| {
            if state.cursor >= char_count(&state.value) {
                return false;
            }
            let start = byte_offset(&state.value, state.cursor);
            let end = byte_offset(&state.value, state.cursor + 1);
            state.value.replace_range(start..end, "");
            true
        });
    }

    /// Delete to the start of the line.
    fn delete_to_line_start(&self) {
        self.edit(false, |state| {
            if state.cursor == 0 {
                return false;
            }
            let at = byte_offset(&state.value, state.cursor);
            let deleted = state.value[..at].to_string();
            if let Ok(mut kill_ring) = self.kill_ring.lock() {
                use super::kill_ring::PushOptions;
                let accumulate = self.get_last_action() == LastAction::Kill;
                kill_ring.push(
                    &deleted,
                    PushOptions {
                        prepend: true,
                        accumulate,
                    },
                );
            }
            self.set_last_action(LastAction::Kill);
            state.value.replace_range(..at, "");
            state.cursor = 0;
            true
        });
    }

    /// Delete to the end of the line.
    fn delete_to_line_end(&self) {
        self.edit(false, |state| {
            if state.cursor >= char_count(&state.value) {
                return false;
            }
            let at = byte_offset(&state.value, state.cursor);
            let deleted = state.value[at..].to_string();
            if let Ok(mut kill_ring) = self.kill_ring.lock() {
                use super::kill_ring::PushOptions;
                let accumulate = self.get_last_action() == LastAction::Kill;
                kill_ring.push(
                    &deleted,
                    PushOptions {
                        prepend: false,
                        accumulate,
                    },
                );
            }
            self.set_last_action(LastAction::Kill);
            state.value.truncate(at);
            true
        });
    }

    /// Delete word backwards.
    fn delete_word_backwards(&self) {
        self.edit(false, |state| {
            if state.cursor == 0 {
                return false;
            }
            let was_kill = self.get_last_action() == LastAction::Kill;
            let old_cursor = state.cursor;
            let new_cursor = find_word_backward(&state.value, old_cursor);
            let start = byte_offset(&state.value, new_cursor);
            let end = byte_offset(&state.value, old_cursor);
            let deleted = state.value[start..end].to_string();
            if let Ok(mut kill_ring) = self.kill_ring.lock() {
                use super::kill_ring::PushOptions;
                kill_ring.push(
                    &deleted,
                    PushOptions {
                        prepend: true,
                        accumulate: was_kill,
                    },
                );
            }
            self.set_last_action(LastAction::Kill);
            state.value.replace_range(start..end, "");
            state.cursor = new_cursor;
            true
        });
    }

    /// Delete word forward.
    fn delete_word_forward(&self) {
        self.edit(false, |state| {
            if state.cursor >= char_count(&state.value) {
                return false;
            }
            let was_kill = self.get_last_action() == LastAction::Kill;
            let old_cursor = state.cursor;
            let new_cursor = find_word_forward(&state.value, old_cursor);
            let start = byte_offset(&state.value, old_cursor);
            let end = byte_offset(&state.value, new_cursor);
            let deleted = state.value[start..end].to_string();
            if let Ok(mut kill_ring) = self.kill_ring.lock() {
                use super::kill_ring::PushOptions;
                kill_ring.push(
                    &deleted,
                    PushOptions {
                        prepend: false,
                        accumulate: was_kill,
                    },
                );
            }
            self.set_last_action(LastAction::Kill);
            state.value.replace_range(start..end, "");
            state.cursor = old_cursor;
            true
        });
    }

    /// Yank from kill ring.
    fn yank(&self) {
        let text = match self.kill_ring.lock() {
            Ok(kill_ring) => kill_ring.peek().map(|s| s.to_string()),
            Err(_) => None,
        };

        if let Some(text) = text {
            self.set_last_action(LastAction::Yank);
            self.edit(false, |state| {
                let at = byte_offset(&state.value, state.cursor);
                state.value.insert_str(at, &text);
                state.cursor += text.chars().count();
                true
            });
        }
    }

    /// Yank pop (cycle through kill ring).
    #[allow(dead_code)]
    fn yank_pop(&self) {
        if self.get_last_action() != LastAction::Yank {
            return;
        }

        // Rotate the kill ring before touching the state lock so the ring is
        // never held across an edit.
        let (previous, replacement) = {
            let Ok(mut kill_ring) = self.kill_ring.lock() else {
                return;
            };
            if kill_ring.len() <= 1 {
                return;
            }
            let previous = kill_ring.peek().unwrap_or("").to_string();
            kill_ring.rotate();
            let replacement = kill_ring.peek().unwrap_or("").to_string();
            (previous, replacement)
        };

        self.edit(false, |state| {
            let previous_len = previous.chars().count();
            if state.cursor < previous_len {
                return false;
            }
            // Remove the previously yanked text, then insert the next candidate.
            let start = byte_offset(&state.value, state.cursor - previous_len);
            let end = byte_offset(&state.value, state.cursor);
            state.value.replace_range(start..end, &replacement);
            state.cursor = state.cursor - previous_len + replacement.chars().count();
            true
        });
        self.set_last_action(LastAction::Yank);
    }

    /// Move cursor left.
    fn move_cursor_left(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
        }
    }

    /// Move cursor right.
    fn move_cursor_right(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            if state.cursor < char_count(&state.value) {
                state.cursor += 1;
            }
        }
    }

    /// Move cursor to line start.
    fn move_to_line_start(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            state.cursor = 0;
        }
    }

    /// Move cursor to line end.
    fn move_to_line_end(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            state.cursor = char_count(&state.value);
        }
    }

    /// Move cursor word left.
    fn move_word_left(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            state.cursor = find_word_backward(&state.value, state.cursor);
        }
    }

    /// Move cursor word right.
    fn move_word_right(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            state.cursor = find_word_forward(&state.value, state.cursor);
        }
    }

    /// Undo.
    fn undo(&self) {
        if let Ok(mut undo_stack) = self.undo_stack.lock() {
            if let Some(snapshot) = undo_stack.pop() {
                drop(undo_stack);
                if let Ok(mut state) = self.state.lock() {
                    *state = snapshot;
                }
                self.set_last_action(LastAction::None);
                self.notify_change();
            }
        }
    }

    /// Apply a mutation to the input state, record an undo snapshot, and notify
    /// listeners.
    ///
    /// The undo snapshot is taken while the state lock is held but pushed to the
    /// undo stack *after* releasing it, and `notify_change` (which calls
    /// `get_value` ⇒ `state.lock()`) runs last. Both used to run nested inside the
    /// state lock, and `std::sync::Mutex` is not reentrant — so the first
    /// backspace (every deletion path, actually) deadlocked the whole TUI and
    /// wedged the `ask_user` freeform input.
    ///
    /// `coalesce` skips the snapshot so consecutive keystrokes of a word merge
    /// into a single undo unit.
    fn edit(&self, coalesce: bool, mutate: impl FnOnce(&mut InputState) -> bool) {
        let snapshot = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let snapshot = if coalesce {
                None
            } else {
                Some(InputState {
                    value: state.value.clone(),
                    cursor: state.cursor,
                })
            };
            if !mutate(&mut state) {
                return;
            }
            snapshot
        };
        if let Some(snapshot) = snapshot {
            if let Ok(mut undo_stack) = self.undo_stack.lock() {
                undo_stack.push(snapshot);
            }
        }
        self.notify_change();
    }

    /// Get the last action.
    fn get_last_action(&self) -> LastAction {
        self.last_action
            .lock()
            .map(|a| *a)
            .unwrap_or(LastAction::None)
    }

    /// Set the last action.
    fn set_last_action(&self, action: LastAction) {
        if let Ok(mut a) = self.last_action.lock() {
            *a = action;
        }
    }

    /// Notify change callback.
    fn notify_change(&self) {
        if let Ok(cb) = self.on_change.lock() {
            if let Some(callback) = cb.as_ref() {
                let value = self.get_value();
                callback(&value);
            }
        }
    }

    /// Handle key input.
    pub fn handle_key(&self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        match (key.code, key.modifiers) {
            // Submit
            (KeyCode::Enter, _) | (KeyCode::Char('\n'), _) => {
                if let Ok(cb) = self.on_submit.lock() {
                    if let Some(callback) = cb.as_ref() {
                        let value = self.get_value();
                        callback(&value);
                    }
                }
            }
            // Escape
            (KeyCode::Esc, _) => {
                if let Ok(cb) = self.on_escape.lock() {
                    if let Some(callback) = cb.as_ref() {
                        callback();
                    }
                }
            }
            // Navigation
            (KeyCode::Left, KeyModifiers::CONTROL) => self.move_word_left(),
            (KeyCode::Right, KeyModifiers::CONTROL) => self.move_word_right(),
            (KeyCode::Left, _) => self.move_cursor_left(),
            (KeyCode::Right, _) => self.move_cursor_right(),
            (KeyCode::Home, _) => self.move_to_line_start(),
            (KeyCode::End, _) => self.move_to_line_end(),
            // Deletion
            (KeyCode::Backspace, KeyModifiers::CONTROL) => self.delete_word_backwards(),
            (KeyCode::Delete, KeyModifiers::CONTROL) => self.delete_word_forward(),
            (KeyCode::Backspace, _) => self.handle_backspace(),
            (KeyCode::Delete, _) => self.handle_forward_delete(),
            // Character input
            (KeyCode::Char(c), KeyModifiers::CONTROL) => match c {
                'a' => self.move_to_line_start(),
                'e' => self.move_to_line_end(),
                'b' => self.move_cursor_left(),
                'f' => self.move_cursor_right(),
                'w' => self.delete_word_backwards(),
                'k' => self.delete_to_line_end(),
                'u' => self.delete_to_line_start(),
                'y' => self.yank(),
                '_' | '/' => self.undo(),
                _ => {}
            },
            (KeyCode::Char(c), KeyModifiers::ALT) => match c {
                'b' => self.move_word_left(),
                'f' => self.move_word_right(),
                'd' => self.delete_word_forward(),
                _ => {}
            },
            (KeyCode::Char(c), _) => {
                self.insert_character(&c.to_string());
            }
            _ => {}
        }
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Input {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let focused = *self.focused.lock().unwrap_or_else(|e| e.into_inner());
        let masked = *self.masked.lock().unwrap_or_else(|e| e.into_inner());

        let prompt_len = visible_width(&self.prompt);
        let available_width = width.saturating_sub(prompt_len);

        if available_width == 0 {
            return vec![self.prompt.clone()];
        }

        let value = &state.value;
        let cursor = state.cursor.min(char_count(value));
        let display_value = if masked {
            "•".repeat(char_count(value))
        } else {
            value.clone()
        };

        // Show placeholder if empty and not focused
        if value.is_empty() && !focused {
            if let Some(ref placeholder) = self.placeholder {
                let dimmed = format!("\x1b[90m{}\x1b[0m", placeholder);
                return vec![format!("{}{}", self.prompt, dimmed)];
            }
        }

        let total_width = visible_width(&display_value);
        let cursor_byte = byte_offset(&display_value, cursor);
        let cursor_col = visible_width(&display_value[..cursor_byte]);

        // Horizontal scrolling. The window is chosen in display columns, but the
        // caret offset inside it must be counted in *characters* — the previous
        // implementation reused the byte cursor as an index into the visible
        // `chars` vector and re-derived it with a quadratic
        // `chars().position()` scan (which finds the FIRST occurrence of a char
        // and feeds that index back in as a byte offset). That desynced the
        // caret as soon as the text scrolled, and panicked on non-ASCII text.
        let (visible_text, cursor_display) = if total_width < available_width {
            (display_value.clone(), cursor)
        } else {
            let scroll_width = if cursor == char_count(value) {
                available_width.saturating_sub(1)
            } else {
                available_width
            };

            if scroll_width == 0 {
                (String::new(), 0)
            } else {
                let half_width = scroll_width / 2;
                let start_col = if cursor_col < half_width {
                    0
                } else if cursor_col > total_width.saturating_sub(half_width) {
                    total_width.saturating_sub(scroll_width)
                } else {
                    cursor_col.saturating_sub(half_width)
                };
                let visible = slice_by_column(&display_value, start_col, scroll_width, true);
                let caret_cols = cursor_col.saturating_sub(start_col).min(scroll_width);
                let before_caret = slice_by_column(&display_value, start_col, caret_cols, true);
                let caret = before_caret.chars().count().min(char_count(&visible));
                (visible, caret)
            }
        };

        // Insert cursor marker and reverse video for cursor character
        let chars: Vec<char> = visible_text.chars().collect();
        let cursor_display = cursor_display.min(chars.len());
        let before_cursor: String = chars.iter().take(cursor_display).collect();
        let at_cursor = chars.get(cursor_display).copied().unwrap_or(' ');
        let after_cursor: String = chars.iter().skip(cursor_display + 1).collect();

        let marker = if focused { CURSOR_MARKER } else { "" };
        let cursor_char = if focused {
            format!("\x1b[7m{}\x1b[27m", at_cursor)
        } else {
            at_cursor.to_string()
        };

        // The marker is zero-width and `extract_cursor_position` derives the
        // hardware cursor column from the visible text *before* it, so it has to
        // sit exactly at the caret. Emitting it right after the prompt (as this
        // used to) pinned the real terminal caret to the start of the input and
        // it never followed the text being typed.
        let line = format!(
            "{}{}{}{}{}{}",
            self.prompt, before_cursor, marker, cursor_char, after_cursor, "\x1b[0m"
        );

        vec![line]
    }

    fn invalidate(&self) {
        // No cached state to invalidate
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Focusable for Input {
    fn set_focused(&self, focused: bool) {
        if let Ok(mut f) = self.focused.lock() {
            *f = focused;
        }
    }

    fn is_focused(&self) -> bool {
        *self.focused.lock().unwrap_or_else(|e| e.into_inner())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_ansi;
    use crossterm::event::{KeyCode, KeyModifiers};

    /// Press one key with no modifiers.
    fn key(input: &Input, code: KeyCode) {
        input.handle_key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn test_input_value() {
        let input = Input::new();
        input.set_value("hello");
        assert_eq!(input.get_value(), "hello");
    }

    #[test]
    fn test_input_clear() {
        let input = Input::new();
        input.set_value("hello");
        input.clear();
        assert_eq!(input.get_value(), "");
    }

    #[test]
    fn test_input_render() {
        let input = Input::new();
        input.set_value("hello");
        let lines = input.render(20);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("hello"));
    }

    #[test]
    fn test_input_placeholder() {
        let input = Input::with_placeholder("Enter text...");
        let lines = input.render(20);
        assert!(lines[0].contains("Enter text..."));
    }

    #[test]
    fn masked_input_hides_value_but_preserves_submission_value() {
        let input = Input::new();
        input.set_focused(true);
        input.set_masked(true);
        input.set_value("secret-value");
        let rendered = strip_ansi(&input.render(40).remove(0));
        assert!(!rendered.contains("secret-value"));
        assert!(rendered.contains("••••••••••••"));
        assert_eq!(input.get_value(), "secret-value");
    }

    /// Regression: every deletion path used to call `push_undo()` and
    /// `notify_change()` *while already holding* the `state` mutex. Both re-lock
    /// it and `std::sync::Mutex` is not reentrant, so the first backspace
    /// deadlocked the whole TUI and wedged the `ask_user` freeform input.
    #[test]
    fn deletion_keys_do_not_deadlock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let press = |code: KeyCode, mods: KeyModifiers| {
            let input = Arc::new(Input::new());
            input.set_value("hello world");
            let (tx, rx) = mpsc::channel();
            let handle = input.clone();
            std::thread::spawn(move || {
                handle.handle_key(crossterm::event::KeyEvent::new(code, mods));
                let _ = tx.send(handle.get_value());
            });
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(value) => value,
                Err(_) => panic!("{code:?} deadlocked"),
            }
        };

        assert_eq!(press(KeyCode::Backspace, KeyModifiers::NONE), "hello worl");
        assert_eq!(press(KeyCode::Delete, KeyModifiers::NONE), "hello world");
        assert_eq!(press(KeyCode::Backspace, KeyModifiers::CONTROL), "hello ");
        // Ctrl+K sits at the end of the line, so it is a no-op; Ctrl+U kills
        // back to the start.
        assert_eq!(
            press(KeyCode::Char('k'), KeyModifiers::CONTROL),
            "hello world"
        );
        assert_eq!(press(KeyCode::Char('u'), KeyModifiers::CONTROL), "");
    }

    /// Regression: `render` emitted `CURSOR_MARKER` right after the prompt, but
    /// `extract_cursor_position` measures the hardware cursor column from the
    /// text *before* the marker — so the real terminal caret stayed pinned at
    /// the start of the input instead of following the document.
    #[test]
    fn cursor_marker_sits_at_the_caret() {
        let input = Input::new();
        input.set_focused(true);
        input.set_value("hello world");
        let line = input.render(40).remove(0);
        let marker_at = line.find(CURSOR_MARKER).expect("marker present");
        // Caret is at the end of "hello world", not at column 0 / after "> ".
        assert_eq!(
            visible_width(&line[..marker_at]),
            visible_width("> hello world"),
            "caret not at end: {line:?}"
        );
    }

    /// Same, but with a value wider than the field so the input has to scroll,
    /// and with multibyte text (the old caret math reused a byte cursor as a
    /// char index plus a quadratic `chars().position()` rescan).
    #[test]
    fn cursor_marker_tracks_scrolled_multibyte_text() {
        let input = Input::new();
        input.set_focused(true);

        let long: String = "0123456789".repeat(6);
        input.set_value(&long);
        let line = input.render(20).remove(0);
        let marker_at = line.find(CURSOR_MARKER).expect("marker present");
        let caret_col = visible_width(&line[..marker_at]);
        let plain = strip_ansi(&line.replace(CURSOR_MARKER, ""));
        assert!(
            caret_col > visible_width("> "),
            "caret stuck at the prompt: col={caret_col}"
        );
        assert!(plain.contains("89"), "window lost the tail: {plain:?}");

        // Multibyte value: must not panic and must keep the caret at the end.
        input.set_value("\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}");
        let line = input.render(40).remove(0);
        let marker_at = line.find(CURSOR_MARKER).expect("marker present");
        assert_eq!(
            visible_width(&line[..marker_at]),
            visible_width("> \u{4e2d}\u{6587}\u{6d4b}\u{8bd5}")
        );
        assert!(strip_ansi(&line).contains("\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}"));
    }

    /// Regression: the cursor is a character index while `&str` is sliced by
    /// byte offset, so `&value[..cursor]` panicked on non-ASCII input and
    /// `set_value` left the cursor past the end of the text.
    #[test]
    fn deletions_are_char_aware() {
        let input = Input::new();

        // set_value must put the cursor at the end, so Backspace deletes.
        input.set_value("ab\u{4e2d}");
        key(&input, KeyCode::Backspace);
        assert_eq!(input.get_value(), "ab");
        key(&input, KeyCode::Backspace);
        assert_eq!(input.get_value(), "a");

        // Forward delete across a multibyte boundary.
        input.set_value("\u{4e2d}b");
        key(&input, KeyCode::Left);
        key(&input, KeyCode::Delete);
        assert_eq!(input.get_value(), "\u{4e2d}");
        key(&input, KeyCode::Left);
        key(&input, KeyCode::Delete);
        assert_eq!(input.get_value(), "");

        // Insertion at the end must not land mid-character.
        input.set_value("\u{4e2d}\u{6587}");
        key(&input, KeyCode::Char('X'));
        assert_eq!(input.get_value(), "\u{4e2d}\u{6587}X");

        // Nor may moving/inserting inside a multibyte string land mid-char.
        input.set_value("a\u{4e2d}b");
        key(&input, KeyCode::Left);
        key(&input, KeyCode::Char('X'));
        assert_eq!(input.get_value(), "a\u{4e2d}Xb");
        key(&input, KeyCode::Backspace);
        assert_eq!(input.get_value(), "a\u{4e2d}b");
        key(&input, KeyCode::Backspace);
        assert_eq!(input.get_value(), "ab");
    }
}
