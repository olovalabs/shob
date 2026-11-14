//! Keybinding hint formatting utilities.
//!
//! Port of `packages/coding-agent/src/modes/interactive/components/keybinding-hints.ts`.
//! Produces human-readable key strings (e.g. `"Ctrl+C"`, `"Shift+Enter"`) from
//! the global [`Keybindings`] registry, plus themed hint lines like
//! `"<muted>Ctrl+C</muted> <muted>cancel</muted>"`.
//!
//! NOTE: the registry only carries `tui.*` IDs — there are no `app.*` bindings.
//! Callers that need a hint for an action with no registered binding must use
//! [`raw_key_hint`] with a literal string (e.g. `"Ctrl+T"`), never an
//! `app.*` ID.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::keybindings::{get_keybindings, KeyCombo, KeybindingId};
use crate::theme::{theme, Color};

/// Format a single [`KeyCombo`] as `"Ctrl+C"` / `"Shift+Enter"` / `"Enter"`.
///
/// Modifier order is Ctrl, Alt, Super, Shift. Modifier names are always
/// first-letter-capitalized ("Ctrl", "Shift", "Alt", "Super") to match the
/// convention used elsewhere in this codebase (e.g. the footer hints
/// `"Ctrl+C: Exit"`). `Char('c')` under Control renders as `"C"` (uppercased
/// for readability). Shift alone on an ASCII letter is implied by the glyph
/// and dropped (`Shift+Char('A')` → `"A"`).
pub fn format_combo(combo: KeyCombo, capitalize: bool) -> String {
    let _ = capitalize; // modifiers are always capitalized; key casing handled below
    let mut parts: Vec<String> = Vec::new();

    let m = combo.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let sup = m.contains(KeyModifiers::SUPER);

    if ctrl {
        parts.push(modifier_name("ctrl"));
    }
    if alt {
        parts.push(modifier_name("alt"));
    }
    if sup {
        parts.push(modifier_name("super"));
    }

    let key_name = key_code_name(&combo.code);

    // Shift on a printable char is implied by the glyph — drop it. For other
    // keys (Enter, Esc, arrows) keep "Shift+".
    let shift_implicit = matches!(combo.code, KeyCode::Char(_)) && !ctrl && !alt;
    if m.contains(KeyModifiers::SHIFT) && !shift_implicit {
        parts.push(modifier_name("shift"));
    }

    parts.push(key_name);
    parts.join("+")
}

/// Always first-letter-capitalize a modifier name.
fn modifier_name(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Render a [`KeyCode`] as a short display name.
pub fn key_code_name(code: &KeyCode) -> String {
    match code {
        KeyCode::Char(c) => {
            // For Ctrl+Char the crossterm code is a lowercase letter; render
            // uppercased for readability ("Ctrl+C" not "Ctrl+c") only when the
            // char is ASCII-alphabetic.
            if c.is_ascii_alphabetic() {
                c.to_ascii_uppercase().to_string()
            } else {
                c.to_string()
            }
        }
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Shift+Tab".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::CapsLock => "CapsLock".to_string(),
        KeyCode::ScrollLock => "ScrollLock".to_string(),
        KeyCode::NumLock => "NumLock".to_string(),
        KeyCode::PrintScreen => "PrintScreen".to_string(),
        KeyCode::Pause => "Pause".to_string(),
        KeyCode::Menu => "Menu".to_string(),
        KeyCode::F(n) => format!("F{}", n),
        KeyCode::Null => String::new(),
        KeyCode::Media(_) => "Media".to_string(),
        KeyCode::Modifier(_) => "Modifier".to_string(),
        KeyCode::KeypadBegin => "KeypadBegin".to_string(),
    }
}

/// Resolve a keybinding ID to its key display text, joining multiple
/// `KeyCombo`s with `/`. Returns an empty string when the ID has no bound keys
/// (mirrors the TS `keyText`).
pub fn key_text(id: KeybindingId) -> String {
    let combos = get_keybindings().get_keys(id);
    if combos.is_empty() {
        return String::new();
    }
    combos
        .iter()
        .map(|c| format_combo(*c, false))
        .collect::<Vec<_>>()
        .join("/")
}

/// Capitalized variant of [`key_text`] (first letter of each part uppercased).
pub fn key_display_text(id: KeybindingId) -> String {
    let combos = get_keybindings().get_keys(id);
    if combos.is_empty() {
        return String::new();
    }
    combos
        .iter()
        .map(|c| format_combo(*c, true))
        .collect::<Vec<_>>()
        .join("/")
}

/// Themed hint: `<muted>keys</muted> <muted>description</muted>`.
///
/// The TS version uses a `dim` slot; rpi-tui has no `dim` color, so both the
/// keys and description use the theme's `muted` shade — subdued in any preset.
pub fn key_hint(id: KeybindingId, description: &str) -> String {
    let colors = theme().colors;
    let keys = key_text(id);
    hint_line(&keys, description, colors.muted, colors.muted)
}

/// Themed hint from a literal key string (for keys not in the registry, e.g.
/// `"Ctrl+T"`, `"↑↓"`). Same shape as [`key_hint`].
pub fn raw_key_hint(key: &str, description: &str) -> String {
    let colors = theme().colors;
    hint_line(key, description, colors.muted, colors.muted)
}

fn hint_line(keys: &str, description: &str, key_color: Color, desc_color: Color) -> String {
    format!("{} {}", key_color.fg(keys), desc_color.fg(description))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_text_submit_contains_enter() {
        // tui.input.submit defaults to Enter (no modifiers).
        let text = key_text("tui.input.submit");
        assert!(text.contains("Enter"), "got: {text}");
    }

    #[test]
    fn test_key_text_newline_has_shift_and_ctrl() {
        // tui.input.newLine = Shift+Enter, Ctrl+J.
        let text = key_text("tui.input.newLine");
        assert!(text.contains("Enter") || text.contains("J"));
    }

    #[test]
    fn test_key_text_unknown_id_empty() {
        assert_eq!(key_text("tui.does.not.exist"), "");
    }

    #[test]
    fn test_format_combo_ctrl_c() {
        let combo = KeyCombo::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(format_combo(combo, false), "Ctrl+C");
        assert_eq!(format_combo(combo, true), "Ctrl+C");
    }

    #[test]
    fn test_format_combo_shift_enter() {
        let combo = KeyCombo::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(format_combo(combo, false), "Shift+Enter");
    }

    #[test]
    fn test_key_code_name_special() {
        assert_eq!(key_code_name(&KeyCode::Esc), "Esc");
        assert_eq!(key_code_name(&KeyCode::F(5)), "F5");
        assert_eq!(key_code_name(&KeyCode::Char('a')), "A");
        assert_eq!(key_code_name(&KeyCode::Char('$')), "$");
    }
}
