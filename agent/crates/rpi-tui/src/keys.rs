//! Keyboard input handling for terminal applications.
//!
//! Supports both legacy terminal sequences and Kitty keyboard protocol.
//! See: https://sw.kovidgoyal.net/kitty/keyboard-protocol/

use std::sync::atomic::{AtomicBool, Ordering};

/// Global Kitty protocol state.
static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set the global Kitty keyboard protocol state.
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::SeqCst);
}

/// Query whether Kitty keyboard protocol is currently active.
pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::SeqCst)
}

/// Key identifier type.
pub type KeyId = String;

/// Modifier names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

impl Modifier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Modifier::Ctrl => "ctrl",
            Modifier::Shift => "shift",
            Modifier::Alt => "alt",
            Modifier::Super => "super",
        }
    }
}

/// A parsed key event.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    /// The base key (e.g., "a", "enter", "f1").
    pub key: String,
    /// Modifiers pressed.
    pub modifiers: Vec<Modifier>,
    /// Event type (press, release, repeat).
    pub event_type: KeyEventType,
}

impl Key {
    /// Create a new key with no modifiers.
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            modifiers: Vec::new(),
            event_type: KeyEventType::Press,
        }
    }

    /// Add a modifier.
    pub fn with_modifier(mut self, modifier: Modifier) -> Self {
        self.modifiers.push(modifier);
        self
    }

    /// Set the event type.
    pub fn with_event_type(mut self, event_type: KeyEventType) -> Self {
        self.event_type = event_type;
        self
    }

    /// Convert to key identifier string.
    pub fn to_key_id(&self) -> KeyId {
        let mut parts: Vec<&str> = self.modifiers.iter().map(|m| m.as_str()).collect();
        parts.sort(); // Sort for consistency
        parts.push(&self.key);
        parts.join("+")
    }

    /// Check if this key matches a key identifier.
    pub fn matches(&self, key_id: &str) -> bool {
        self.to_key_id() == key_id
    }
}

/// Key event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyEventType {
    Press,
    Release,
    Repeat,
}

/// Helper struct for creating typed key identifiers.
pub struct KeyHelper;

impl KeyHelper {
    // Special keys
    pub const ESCAPE: &'static str = "escape";
    pub const ESC: &'static str = "esc";
    pub const ENTER: &'static str = "enter";
    pub const RETURN: &'static str = "return";
    pub const TAB: &'static str = "tab";
    pub const SPACE: &'static str = "space";
    pub const BACKSPACE: &'static str = "backspace";
    pub const DELETE: &'static str = "delete";
    pub const INSERT: &'static str = "insert";
    pub const CLEAR: &'static str = "clear";
    pub const HOME: &'static str = "home";
    pub const END: &'static str = "end";
    pub const PAGE_UP: &'static str = "pageUp";
    pub const PAGE_DOWN: &'static str = "pageDown";
    pub const UP: &'static str = "up";
    pub const DOWN: &'static str = "down";
    pub const LEFT: &'static str = "left";
    pub const RIGHT: &'static str = "right";

    // Function keys
    pub const F1: &'static str = "f1";
    pub const F2: &'static str = "f2";
    pub const F3: &'static str = "f3";
    pub const F4: &'static str = "f4";
    pub const F5: &'static str = "f5";
    pub const F6: &'static str = "f6";
    pub const F7: &'static str = "f7";
    pub const F8: &'static str = "f8";
    pub const F9: &'static str = "f9";
    pub const F10: &'static str = "f10";
    pub const F11: &'static str = "f11";
    pub const F12: &'static str = "f12";

    // Symbol keys
    pub const BACKTICK: &'static str = "`";
    pub const HYPHEN: &'static str = "-";
    pub const EQUALS: &'static str = "=";
    pub const LEFT_BRACKET: &'static str = "[";
    pub const RIGHT_BRACKET: &'static str = "]";
    pub const BACKSLASH: &'static str = "\\";
    pub const SEMICOLON: &'static str = ";";
    pub const QUOTE: &'static str = "'";
    pub const COMMA: &'static str = ",";
    pub const PERIOD: &'static str = ".";
    pub const SLASH: &'static str = "/";

    // Single modifier helpers
    pub fn ctrl(key: &str) -> String {
        format!("ctrl+{}", key)
    }

    pub fn shift(key: &str) -> String {
        format!("shift+{}", key)
    }

    pub fn alt(key: &str) -> String {
        format!("alt+{}", key)
    }

    pub fn super_key(key: &str) -> String {
        format!("super+{}", key)
    }

    // Combined modifier helpers
    pub fn ctrl_shift(key: &str) -> String {
        format!("ctrl+shift+{}", key)
    }

    pub fn ctrl_alt(key: &str) -> String {
        format!("ctrl+alt+{}", key)
    }

    pub fn alt_shift(key: &str) -> String {
        format!("alt+shift+{}", key)
    }
}

/// Symbol keys set.
const SYMBOL_KEYS: &[&str] = &[
    "`", "-", "=", "[", "]", "\\", ";", "'", ",", ".", "/", "!", "@", "#", "$", "%", "^", "&", "*",
    "(", ")", "_", "+", "|", "~", "{", "}", ":", "<", ">", "?",
];

/// Check if a key is a symbol key.
pub fn is_symbol_key(key: &str) -> bool {
    SYMBOL_KEYS.contains(&key)
}

/// Modifier bit flags.
const MOD_CTRL: u8 = 1;
const MOD_SHIFT: u8 = 2;
const MOD_ALT: u8 = 4;
const MOD_SUPER: u8 = 8;
#[allow(dead_code)]
const LOCK_MASK: u8 = 64 + 128; // Caps Lock + Num Lock

/// Parse a key event from terminal input data.
/// Returns the parsed key if the data represents a key event.
pub fn parse_key(data: &str) -> Option<Key> {
    if data.is_empty() {
        return None;
    }

    // Single character (including control characters)
    if data.len() == 1 {
        let c = data.chars().next()?;
        return parse_single_char(c);
    }

    // Check for escape sequences
    if data.starts_with('\x1b') {
        return parse_escape_sequence(data);
    }

    None
}

/// Parse a single character key.
fn parse_single_char(c: char) -> Option<Key> {
    let code = c as u32;

    // Control characters
    match code {
        0x00 => return Some(Key::new("ctrl+space")),
        0x01..=0x07 => {
            // Ctrl+A to Ctrl+G
            let letter = (b'a' + (code - 1) as u8) as char;
            return Some(Key::new(KeyHelper::ctrl(&letter.to_string())));
        }
        0x08 => return Some(Key::new(KeyHelper::BACKSPACE)), // Ctrl+H = Backspace
        0x09 => return Some(Key::new(KeyHelper::TAB)),       // Ctrl+I = Tab
        0x0a | 0x0d => return Some(Key::new(KeyHelper::ENTER)), // Ctrl+J/M = Enter
        0x0b..=0x1a => {
            // Ctrl+K to Ctrl+Z
            let letter = (b'k' + (code - 0x0b) as u8) as char;
            return Some(Key::new(KeyHelper::ctrl(&letter.to_string())));
        }
        0x1b => return Some(Key::new(KeyHelper::ESCAPE)),
        0x7f => return Some(Key::new(KeyHelper::BACKSPACE)), // DEL
        _ => {}
    }

    // Regular printable character
    if c >= ' ' && c != '\x7f' {
        let key_str = c.to_string();
        if key_str.chars().all(|c| c.is_uppercase()) {
            Some(Key::new(key_str.to_lowercase()).with_modifier(Modifier::Shift))
        } else {
            Some(Key::new(key_str))
        }
    } else {
        None
    }
}

/// Parse an escape sequence.
fn parse_escape_sequence(data: &str) -> Option<Key> {
    let bytes = data.as_bytes();

    // CSI sequence: ESC [
    if bytes.len() >= 3 && bytes[1] == b'[' {
        return parse_csi_sequence(data);
    }

    // SS3 sequence: ESC O
    if bytes.len() >= 3 && bytes[1] == b'O' {
        return parse_ss3_sequence(data);
    }

    // Alt + key (ESC followed by single char)
    if bytes.len() == 2 {
        let c = bytes[1] as char;
        let mut key = parse_single_char(c)?;
        key.modifiers.push(Modifier::Alt);
        return Some(key);
    }

    None
}

/// Parse a CSI sequence.
fn parse_csi_sequence(data: &str) -> Option<Key> {
    let bytes = data.as_bytes();

    // Find the final byte (0x40-0x7E)
    let final_idx = bytes.iter().rposition(|&b| (0x40..=0x7E).contains(&b))?;
    let final_byte = bytes[final_idx];

    // Parse parameters (between [ and final byte)
    let params_str = &data[2..final_idx];
    let params: Vec<u32> = params_str
        .split(';')
        .filter_map(|s| s.parse().ok())
        .collect();

    // Decode modifiers
    let mut modifiers = Vec::new();
    if params.len() >= 2 {
        let mod_byte = params[1] as u8;
        if mod_byte & MOD_CTRL != 0 {
            modifiers.push(Modifier::Ctrl);
        }
        if mod_byte & MOD_SHIFT != 0 {
            modifiers.push(Modifier::Shift);
        }
        if mod_byte & MOD_ALT != 0 {
            modifiers.push(Modifier::Alt);
        }
        if mod_byte & MOD_SUPER != 0 {
            modifiers.push(Modifier::Super);
        }
    }

    // Determine event type (Kitty protocol)
    let event_type = if params.len() >= 2 {
        let event_type_code = params.first().copied().unwrap_or(1);
        match event_type_code {
            1 => KeyEventType::Press,
            2 => KeyEventType::Repeat,
            3 => KeyEventType::Release,
            _ => KeyEventType::Press,
        }
    } else {
        KeyEventType::Press
    };

    // Parse key based on final byte and parameters
    let key = match final_byte {
        b'~' => parse_csi_tilde_key(&params)?,
        b'A' => Key::new(KeyHelper::UP),
        b'B' => Key::new(KeyHelper::DOWN),
        b'C' => Key::new(KeyHelper::RIGHT),
        b'D' => Key::new(KeyHelper::LEFT),
        b'E' => Key::new("begin"),
        b'F' => Key::new(KeyHelper::END),
        b'H' => Key::new(KeyHelper::HOME),
        b'Z' => Key::new(KeyHelper::TAB).with_modifier(Modifier::Shift),
        b'u' => {
            // Kitty CSI-u: ESC [ code ; modifiers u
            decode_kitty_printable(&params)?
        }
        _ => return None,
    };

    Some(key).map(|mut k| {
        k.modifiers.extend(modifiers);
        k.event_type = event_type;
        k
    })
}

/// Parse CSI tilde key (~ sequences).
fn parse_csi_tilde_key(params: &[u32]) -> Option<Key> {
    let code = params.first().copied().unwrap_or(0);
    match code {
        1 => Some(Key::new(KeyHelper::F1)),
        2 => Some(Key::new(KeyHelper::INSERT)),
        3 => Some(Key::new(KeyHelper::DELETE)),
        4 => Some(Key::new(KeyHelper::END)),
        5 => Some(Key::new(KeyHelper::PAGE_UP)),
        6 => Some(Key::new(KeyHelper::PAGE_DOWN)),
        7 => Some(Key::new(KeyHelper::HOME)),
        8 => Some(Key::new(KeyHelper::END)),
        11..=14 => Some(Key::new(KeyHelper::F1)),
        15..=18 => Some(Key::new(KeyHelper::F5)),
        19..=20 => Some(Key::new(KeyHelper::F9)),
        21..=24 => Some(Key::new(KeyHelper::F11)),
        _ => None,
    }
}

/// Parse SS3 sequence (ESC O sequences).
fn parse_ss3_sequence(data: &str) -> Option<Key> {
    let final_char = data.chars().nth(2)?;
    match final_char {
        'A' => Some(Key::new(KeyHelper::UP)),
        'B' => Some(Key::new(KeyHelper::DOWN)),
        'C' => Some(Key::new(KeyHelper::RIGHT)),
        'D' => Some(Key::new(KeyHelper::LEFT)),
        'E' => Some(Key::new("begin")),
        'F' => Some(Key::new(KeyHelper::END)),
        'H' => Some(Key::new(KeyHelper::HOME)),
        'P' => Some(Key::new(KeyHelper::F1)),
        'Q' => Some(Key::new(KeyHelper::F2)),
        'R' => Some(Key::new(KeyHelper::F3)),
        'S' => Some(Key::new(KeyHelper::F4)),
        _ => None,
    }
}

/// Decode Kitty printable character (CSI-u).
fn decode_kitty_printable(params: &[u32]) -> Option<Key> {
    let code = params.first().copied().unwrap_or(0);

    // Printable ASCII
    if (32..127).contains(&code) {
        let c = code as u8 as char;
        return Some(Key::new(c.to_string()));
    }

    // Decode special characters
    match code {
        57399 => Some(Key::new(KeyHelper::INSERT)),
        57414 => Some(Key::new(KeyHelper::DELETE)),
        57415 => Some(Key::new(KeyHelper::PAGE_UP)),
        57416 => Some(Key::new(KeyHelper::PAGE_DOWN)),
        57399..=57428 => {
            // Function keys in Kitty
            let offset = code - 57399;
            let fn_key = match offset {
                0 => "f1",
                1 => "f2",
                2 => "f3",
                3 => "f4",
                4 => "f5",
                5 => "f6",
                6 => "f7",
                7 => "f8",
                8 => "f9",
                9 => "f10",
                10 => "f11",
                11 => "f12",
                _ => return None,
            };
            Some(Key::new(fn_key))
        }
        _ => None,
    }
}

/// Check if input data matches a key identifier.
pub fn matches_key(data: &str, key_id: &str) -> bool {
    parse_key(data).map(|k| k.matches(key_id)).unwrap_or(false)
}

/// Check if this is a key release event.
pub fn is_key_release(data: &str) -> bool {
    parse_key(data)
        .map(|k| k.event_type == KeyEventType::Release)
        .unwrap_or(false)
}

/// Check if this is a key repeat event.
pub fn is_key_repeat(data: &str) -> bool {
    parse_key(data)
        .map(|k| k.event_type == KeyEventType::Repeat)
        .unwrap_or(false)
}

/// Decode Kitty printable character from raw input.
/// Returns the printable character if this is a Kitty CSI-u sequence.
pub fn decode_kitty_printable_from_str(data: &str) -> Option<String> {
    let bytes = data.as_bytes();

    // Must be a CSI sequence ending with 'u'
    if data.starts_with("\x1b[") && bytes.last()? == &b'u' {
        // Parse parameters
        let params_str = &data[2..data.len() - 1];
        let params: Vec<u32> = params_str
            .split(';')
            .filter_map(|s| s.parse().ok())
            .collect();

        let code = params.first().copied().unwrap_or(0);

        // Printable ASCII
        if (32..127).contains(&code) {
            let c = code as u8 as char;
            return Some(c.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_char() {
        let key = parse_key("a").unwrap();
        assert_eq!(key.key, "a");
        assert_eq!(key.to_key_id(), "a");
    }

    #[test]
    fn test_ctrl_char() {
        let key = parse_key("\x01").unwrap(); // Ctrl+A
        assert_eq!(key.to_key_id(), "ctrl+a");
    }

    #[test]
    fn test_escape() {
        let key = parse_key("\x1b").unwrap();
        assert_eq!(key.key, KeyHelper::ESCAPE);
    }

    #[test]
    fn test_arrow_keys() {
        let key = parse_key("\x1b[A").unwrap();
        assert_eq!(key.key, KeyHelper::UP);

        let key = parse_key("\x1b[B").unwrap();
        assert_eq!(key.key, KeyHelper::DOWN);
    }

    #[test]
    fn test_key_helper() {
        assert_eq!(KeyHelper::ctrl("c"), "ctrl+c");
        assert_eq!(KeyHelper::ctrl_shift("p"), "ctrl+shift+p");
    }

    #[test]
    fn test_matches_key() {
        assert!(matches_key("a", "a"));
        assert!(matches_key("\x01", "ctrl+a"));
        assert!(matches_key("\x1b[A", "up"));
    }
}
