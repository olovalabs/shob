use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

#[cfg(windows)]
pub fn is_capslock_on() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetKeyState(nVirtKey: i32) -> i16;
    }
    const VK_CAPITAL: i32 = 0x14;
    unsafe { (GetKeyState(VK_CAPITAL) & 1) != 0 }
}

#[cfg(not(windows))]
pub fn is_capslock_on() -> bool {
    false
}

pub fn keystroke_to_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    keystroke_to_bytes_internal(keystroke, mode, is_capslock_on())
}

pub fn keystroke_to_bytes_with_caps(
    keystroke: &Keystroke,
    mode: TermMode,
    is_caps: bool,
) -> Option<Vec<u8>> {
    keystroke_to_bytes_internal(keystroke, mode, is_caps)
}

fn keystroke_to_bytes_internal(
    keystroke: &Keystroke,
    mode: TermMode,
    is_caps: bool,
) -> Option<Vec<u8>> {
    let key_lower = keystroke.key.to_ascii_lowercase();

    let mod_code = 1
        + (if keystroke.modifiers.shift { 1 } else { 0 })
        + (if keystroke.modifiers.alt { 2 } else { 0 })
        + (if keystroke.modifiers.control { 4 } else { 0 });

    match key_lower.as_str() {
        "space" => {
            if keystroke.modifiers.control {
                return Some(vec![0x00]);
            }
            return Some(b" ".to_vec());
        }
        "enter" | "return" => return Some(b"\r".to_vec()),
        "escape" => return Some(b"\x1b".to_vec()),
        "tab" => {
            if keystroke.modifiers.shift {
                return Some(b"\x1b[Z".to_vec());
            }
            return Some(b"\t".to_vec());
        }
        "backspace" => {
            if keystroke.modifiers.alt {
                return Some(b"\x1b\x7f".to_vec());
            }
            if keystroke.modifiers.control {
                return Some(vec![0x08]);
            }
            return Some(b"\x7f".to_vec());
        }

        "up" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}A").into_bytes());
            }
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOA".to_vec());
            }
            return Some(b"\x1b[A".to_vec());
        }
        "down" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}B").into_bytes());
            }
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOB".to_vec());
            }
            return Some(b"\x1b[B".to_vec());
        }
        "right" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}C").into_bytes());
            }
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOC".to_vec());
            }
            return Some(b"\x1b[C".to_vec());
        }
        "left" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}D").into_bytes());
            }
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOD".to_vec());
            }
            return Some(b"\x1b[D".to_vec());
        }

        "home" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}H").into_bytes());
            }
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOH".to_vec());
            }
            return Some(b"\x1b[H".to_vec());
        }
        "end" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}F").into_bytes());
            }
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOF".to_vec());
            }
            return Some(b"\x1b[F".to_vec());
        }
        "pageup" => {
            if mod_code > 1 {
                return Some(format!("\x1b[5;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[5~".to_vec());
        }
        "pagedown" => {
            if mod_code > 1 {
                return Some(format!("\x1b[6;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[6~".to_vec());
        }
        "insert" => {
            if mod_code > 1 {
                return Some(format!("\x1b[2;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[2~".to_vec());
        }
        "delete" => {
            if mod_code > 1 {
                return Some(format!("\x1b[3;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[3~".to_vec());
        }

        "f1" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}P").into_bytes());
            }
            return Some(b"\x1bOP".to_vec());
        }
        "f2" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}Q").into_bytes());
            }
            return Some(b"\x1bOQ".to_vec());
        }
        "f3" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}R").into_bytes());
            }
            return Some(b"\x1bOR".to_vec());
        }
        "f4" => {
            if mod_code > 1 {
                return Some(format!("\x1b[1;{mod_code}S").into_bytes());
            }
            return Some(b"\x1bOS".to_vec());
        }

        "f5" => {
            if mod_code > 1 {
                return Some(format!("\x1b[15;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[15~".to_vec());
        }
        "f6" => {
            if mod_code > 1 {
                return Some(format!("\x1b[17;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[17~".to_vec());
        }
        "f7" => {
            if mod_code > 1 {
                return Some(format!("\x1b[18;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[18~".to_vec());
        }
        "f8" => {
            if mod_code > 1 {
                return Some(format!("\x1b[19;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[19~".to_vec());
        }
        "f9" => {
            if mod_code > 1 {
                return Some(format!("\x1b[20;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[20~".to_vec());
        }
        "f10" => {
            if mod_code > 1 {
                return Some(format!("\x1b[21;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[21~".to_vec());
        }
        "f11" => {
            if mod_code > 1 {
                return Some(format!("\x1b[23;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[23~".to_vec());
        }
        "f12" => {
            if mod_code > 1 {
                return Some(format!("\x1b[24;{mod_code}~").into_bytes());
            }
            return Some(b"\x1b[24~".to_vec());
        }

        _ => {}
    }

    if keystroke.modifiers.control && !keystroke.modifiers.alt {
        let key = keystroke.key.as_str();
        if key.len() == 1 {
            let ch = key.chars().next().unwrap();
            if ch.is_ascii_alphabetic() {
                let upper = ch.to_ascii_uppercase();
                let ctrl_char = (upper as u8) - b'@';
                return Some(vec![ctrl_char]);
            }

            match ch {
                '[' => return Some(b"\x1b".to_vec()),
                '\\' => return Some(b"\x1c".to_vec()),
                ']' => return Some(b"\x1d".to_vec()),
                '^' => return Some(b"\x1e".to_vec()),
                '_' => return Some(b"\x1f".to_vec()),
                '?' => return Some(b"\x7f".to_vec()),
                _ => {}
            }
        }
    }

    if keystroke.modifiers.alt && !keystroke.modifiers.control {
        let key = keystroke.key.as_str();
        if key.len() == 1 {
            let ch = key.chars().next().unwrap();
            if ch.is_ascii() {
                return Some(vec![b'\x1b', ch as u8]);
            }
        }
    }

    if !keystroke.modifiers.control && !keystroke.modifiers.alt {
        let should_uppercase = if is_caps {
            keystroke.modifiers.shift ^ true
        } else {
            keystroke.modifiers.shift
                || keystroke
                    .key_char
                    .as_ref()
                    .map_or(false, |s| s.chars().any(|c| c.is_ascii_uppercase()))
        };

        let key = keystroke.key.as_str();
        if key.len() == 1 {
            let ch = key.chars().next().unwrap();
            if ch.is_ascii_alphabetic() {
                let out_char = if should_uppercase {
                    ch.to_ascii_uppercase()
                } else {
                    ch.to_ascii_lowercase()
                };
                return Some(vec![out_char as u8]);
            }
        }

        if let Some(key_char) = &keystroke.key_char {
            if key_char.len() == 1 {
                let ch = key_char.chars().next().unwrap();
                if ch.is_ascii_alphabetic() {
                    let out_char = if should_uppercase {
                        ch.to_ascii_uppercase()
                    } else {
                        ch.to_ascii_lowercase()
                    };
                    return Some(vec![out_char as u8]);
                }
            }
            if !key_char.is_empty() {
                return Some(key_char.as_bytes().to_vec());
            }
        }

        if key.len() == 1 {
            let ch = key.chars().next().unwrap();
            if keystroke.modifiers.shift {
                let shifted = match ch {
                    '1' => '!',
                    '2' => '@',
                    '3' => '#',
                    '4' => '$',
                    '5' => '%',
                    '6' => '^',
                    '7' => '&',
                    '8' => '*',
                    '9' => '(',
                    '0' => ')',
                    '-' => '_',
                    '=' => '+',
                    '[' => '{',
                    ']' => '}',
                    '\\' => '|',
                    ';' => ':',
                    '\'' => '"',
                    ',' => '<',
                    '.' => '>',
                    '/' => '?',
                    '`' => '~',
                    _ if ch.is_ascii_alphabetic() => ch.to_ascii_uppercase(),
                    _ => ch,
                };
                return Some(vec![shifted as u8]);
            }

            if ch.is_ascii() {
                return Some(vec![ch as u8]);
            }

            return Some(key.as_bytes().to_vec());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enter_key() {
        let keystroke = Keystroke::parse("enter").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn test_escape_key() {
        let keystroke = Keystroke::parse("escape").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b".to_vec()));
    }

    #[test]
    fn test_backspace_key() {
        let keystroke = Keystroke::parse("backspace").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x7f".to_vec()));
    }

    #[test]
    fn test_tab_key() {
        let keystroke = Keystroke::parse("tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\t".to_vec()));
    }

    #[test]
    fn test_shift_tab() {
        let keystroke = Keystroke::parse("shift-tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn test_arrow_keys_normal_mode() {
        let mode = TermMode::empty();

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1b[A".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1b[B".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1b[C".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn test_arrow_keys_app_cursor_mode() {
        let mode = TermMode::APP_CURSOR;

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1bOA".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1bOB".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1bOC".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1bOD".to_vec()));
    }

    #[test]
    fn test_navigation_keys() {
        let mode = TermMode::empty();

        let home = Keystroke::parse("home").unwrap();
        assert_eq!(keystroke_to_bytes(&home, mode), Some(b"\x1b[H".to_vec()));

        let end = Keystroke::parse("end").unwrap();
        assert_eq!(keystroke_to_bytes(&end, mode), Some(b"\x1b[F".to_vec()));

        let pageup = Keystroke::parse("pageup").unwrap();
        assert_eq!(keystroke_to_bytes(&pageup, mode), Some(b"\x1b[5~".to_vec()));

        let pagedown = Keystroke::parse("pagedown").unwrap();
        assert_eq!(
            keystroke_to_bytes(&pagedown, mode),
            Some(b"\x1b[6~".to_vec())
        );

        let insert = Keystroke::parse("insert").unwrap();
        assert_eq!(keystroke_to_bytes(&insert, mode), Some(b"\x1b[2~".to_vec()));

        let delete = Keystroke::parse("delete").unwrap();
        assert_eq!(keystroke_to_bytes(&delete, mode), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn test_function_keys() {
        let mode = TermMode::empty();

        let f1 = Keystroke::parse("f1").unwrap();
        assert_eq!(keystroke_to_bytes(&f1, mode), Some(b"\x1bOP".to_vec()));

        let f2 = Keystroke::parse("f2").unwrap();
        assert_eq!(keystroke_to_bytes(&f2, mode), Some(b"\x1bOQ".to_vec()));

        let f5 = Keystroke::parse("f5").unwrap();
        assert_eq!(keystroke_to_bytes(&f5, mode), Some(b"\x1b[15~".to_vec()));

        let f12 = Keystroke::parse("f12").unwrap();
        assert_eq!(keystroke_to_bytes(&f12, mode), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn test_ctrl_combinations() {
        let mode = TermMode::empty();

        let ctrl_a = Keystroke::parse("ctrl-a").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_a, mode), Some(vec![0x01]));

        let ctrl_c = Keystroke::parse("ctrl-c").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_c, mode), Some(vec![0x03]));

        let ctrl_z = Keystroke::parse("ctrl-z").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_z, mode), Some(vec![0x1a]));

        let ctrl_space = Keystroke::parse("ctrl-space").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_space, mode), Some(vec![0x00]));
    }

    #[test]
    fn test_alt_combinations() {
        let mode = TermMode::empty();

        let alt_a = Keystroke::parse("alt-a").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_a, mode), Some(b"\x1ba".to_vec()));

        let alt_x = Keystroke::parse("alt-x").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_x, mode), Some(b"\x1bx".to_vec()));
    }

    #[test]
    fn test_regular_characters() {
        let mode = TermMode::empty();

        let a = Keystroke::parse("a").unwrap();
        assert_eq!(keystroke_to_bytes(&a, mode), Some(b"a".to_vec()));

        let z = Keystroke::parse("z").unwrap();
        assert_eq!(keystroke_to_bytes(&z, mode), Some(b"z".to_vec()));

        let zero = Keystroke::parse("0").unwrap();
        assert_eq!(keystroke_to_bytes(&zero, mode), Some(b"0".to_vec()));
    }

    #[test]
    fn test_space_key() {
        let mode = TermMode::empty();

        let space = Keystroke::parse("space").unwrap();
        assert_eq!(keystroke_to_bytes(&space, mode), Some(b" ".to_vec()));
    }

    #[test]
    fn test_modified_arrow_keys() {
        let mode = TermMode::empty();

        let shift_up = Keystroke::parse("shift-up").unwrap();
        assert_eq!(
            keystroke_to_bytes(&shift_up, mode),
            Some(b"\x1b[1;2A".to_vec())
        );

        let alt_down = Keystroke::parse("alt-down").unwrap();
        assert_eq!(
            keystroke_to_bytes(&alt_down, mode),
            Some(b"\x1b[1;3B".to_vec())
        );

        let ctrl_left = Keystroke::parse("ctrl-left").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_left, mode),
            Some(b"\x1b[1;5D".to_vec())
        );

        let ctrl_right = Keystroke::parse("ctrl-right").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_right, mode),
            Some(b"\x1b[1;5C".to_vec())
        );

        let ctrl_shift_right = Keystroke::parse("ctrl-shift-right").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_shift_right, mode),
            Some(b"\x1b[1;6C".to_vec())
        );
    }

    #[test]
    fn test_modified_navigation_and_fn_keys() {
        let mode = TermMode::empty();

        let ctrl_home = Keystroke::parse("ctrl-home").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_home, mode),
            Some(b"\x1b[1;5H".to_vec())
        );

        let shift_f1 = Keystroke::parse("shift-f1").unwrap();
        assert_eq!(
            keystroke_to_bytes(&shift_f1, mode),
            Some(b"\x1b[1;2P".to_vec())
        );

        let ctrl_f5 = Keystroke::parse("ctrl-f5").unwrap();
        assert_eq!(
            keystroke_to_bytes(&ctrl_f5, mode),
            Some(b"\x1b[15;5~".to_vec())
        );
    }

    #[test]
    fn test_shifted_symbols_fallback() {
        let mode = TermMode::empty();

        let shift_1 = Keystroke::parse("shift-1").unwrap();
        assert_eq!(keystroke_to_bytes(&shift_1, mode), Some(b"!".to_vec()));

        let shift_dash = Keystroke::parse("shift--").unwrap();
        assert_eq!(keystroke_to_bytes(&shift_dash, mode), Some(b"_".to_vec()));

        let shift_slash = Keystroke::parse("shift-/").unwrap();
        assert_eq!(keystroke_to_bytes(&shift_slash, mode), Some(b"?".to_vec()));
    }

    #[test]
    fn test_caps_lock_capitalization() {
        let mode = TermMode::empty();

        let a = Keystroke::parse("a").unwrap();
        assert_eq!(
            keystroke_to_bytes_with_caps(&a, mode, true),
            Some(b"A".to_vec())
        );

        let z = Keystroke::parse("z").unwrap();
        assert_eq!(
            keystroke_to_bytes_with_caps(&z, mode, true),
            Some(b"Z".to_vec())
        );

        let shift_a = Keystroke::parse("shift-a").unwrap();
        assert_eq!(
            keystroke_to_bytes_with_caps(&shift_a, mode, true),
            Some(b"a".to_vec())
        );

        assert_eq!(
            keystroke_to_bytes_with_caps(&shift_a, mode, false),
            Some(b"A".to_vec())
        );

        assert_eq!(
            keystroke_to_bytes_with_caps(&a, mode, false),
            Some(b"a".to_vec())
        );

        let one = Keystroke::parse("1").unwrap();
        assert_eq!(
            keystroke_to_bytes_with_caps(&one, mode, true),
            Some(b"1".to_vec())
        );
    }

    #[test]
    fn test_key_char_with_caps_lock() {
        let mode = TermMode::empty();
        let mut keystroke = Keystroke::parse("a").unwrap();
        keystroke.key_char = Some("a".to_string());

        assert_eq!(
            keystroke_to_bytes_with_caps(&keystroke, mode, true),
            Some(b"A".to_vec())
        );
    }
}
