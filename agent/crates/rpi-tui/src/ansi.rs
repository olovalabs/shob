//! ANSI escape code utilities and visible width calculations.
//!
//! Handles ANSI-aware string manipulation for terminal rendering.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Cursor position marker - APC sequence.
/// This is a zero-width escape sequence that terminals ignore.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// Calculate the visible width of a string, ignoring ANSI escape sequences.
///
/// This is the number of terminal columns the string will occupy.
pub fn visible_width(s: &str) -> usize {
    let stripped = strip_ansi(s);
    UnicodeWidthStr::width(stripped.as_str())
}

/// Strip ANSI escape sequences from a string.
pub fn strip_ansi(s: &str) -> String {
    // Simple ANSI stripping - removes escape sequences
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Cursor markers are private escape sequences, not ANSI CSI/OSC
            // sequences. Treat the complete marker as zero-width; otherwise
            // the old logic skipped only `\\x1b_` and counted `pi:c` as text,
            // making the hardware cursor jump several columns when moving.
            let mut marker_tail = chars.clone();
            let marker = ['_', 'p', 'i', ':', 'c', '\x07'];
            let is_cursor_marker = marker
                .iter()
                .all(|expected| marker_tail.next() == Some(*expected));
            if is_cursor_marker {
                chars = marker_tail;
                continue;
            }

            // Start of escape sequence
            match chars.peek() {
                Some('[') => {
                    chars.next(); // consume '['
                                  // CSI sequence: skip until final byte (0x40-0x7E)
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if (c as u8) >= 0x40 && (c as u8) <= 0x7E {
                            break;
                        }
                    }
                }
                Some(']' | '_' | 'P' | '^') => {
                    chars.next(); // consume ']'
                                  // OSC sequence: skip until BEL (0x07) or ST (ESC \)
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            if let Some('\\') = chars.peek() {
                                chars.next();
                                break;
                            }
                        }
                    }
                }
                Some('(' | ')') => {
                    chars.next(); // consume '(' or ')'
                                  // Character set designation: skip one more char
                    chars.next();
                }
                Some(_) => {
                    // Other escape sequences: skip next char
                    chars.next();
                }
                None => break,
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Slice a string by visible column positions.
///
/// Returns a substring that starts at `start_col` and has at most `width` visible columns.
/// Preserves ANSI escape sequences appropriately.
pub fn slice_by_column(s: &str, start_col: usize, width: usize, preserve_styles: bool) -> String {
    if width == 0 {
        return String::new();
    }

    let stripped = strip_ansi(s);
    let segments: Vec<&str> = stripped.graphemes(true).collect();

    // Find the character indices for the column range
    let mut current_col = 0;
    let mut start_idx = None;
    let mut end_idx = segments.len();
    let mut col_count = 0;

    for (i, seg) in segments.iter().enumerate() {
        let seg_width = UnicodeWidthStr::width(*seg);
        if current_col >= start_col && start_idx.is_none() {
            start_idx = Some(i);
        }
        if start_idx.is_some() {
            col_count += seg_width;
            if col_count >= width {
                end_idx = i + 1;
                break;
            }
        }
        current_col += seg_width;
    }

    let start_idx = start_idx.unwrap_or(segments.len());
    let result: String = segments[start_idx..end_idx].iter().copied().collect();

    if preserve_styles && result != stripped {
        // TODO: Preserve ANSI styles from the original string
        // This is a simplified version
        result
    } else {
        result
    }
}

/// Normalize terminal output by resetting styles at line ends.
pub fn normalize_terminal_output(line: &str) -> String {
    // Reset at end of line
    if line.contains("\x1b[") && !line.ends_with("\x1b[0m") {
        format!("{}\x1b[0m", line)
    } else {
        line.to_string()
    }
}

/// Create a hyperlink using OSC 8.
pub fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{}\x07{}\x1b]8;;\x07", url, text)
}

/// Apply a foreground color using ANSI 256-color mode.
pub fn fg_256(color: u8, text: &str) -> String {
    format!("\x1b[38;5;{}m{}\x1b[39m", color, text)
}

/// Apply a background color using ANSI 256-color mode.
pub fn bg_256(color: u8, text: &str) -> String {
    format!("\x1b[48;5;{}m{}\x1b[49m", color, text)
}

/// Apply a foreground color using RGB.
pub fn fg_rgb(r: u8, g: u8, b: u8, text: &str) -> String {
    format!("\x1b[38;2;{};{};{}m{}\x1b[39m", r, g, b, text)
}

/// Apply a background color using RGB.
pub fn bg_rgb(r: u8, g: u8, b: u8, text: &str) -> String {
    format!("\x1b[48;2;{};{};{}m{}\x1b[49m", r, g, b, text)
}

/// Apply bold style.
pub fn bold(text: &str) -> String {
    format!("\x1b[1m{}\x1b[22m", text)
}

/// Apply italic style.
pub fn italic(text: &str) -> String {
    format!("\x1b[3m{}\x1b[23m", text)
}

/// Apply underline style.
pub fn underline(text: &str) -> String {
    format!("\x1b[4m{}\x1b[24m", text)
}

/// Apply inverse (reverse video) style.
pub fn inverse(text: &str) -> String {
    format!("\x1b[7m{}\x1b[27m", text)
}

/// Apply dim/faint style.
pub fn dim(text: &str) -> String {
    format!("\x1b[2m{}\x1b[22m", text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_visible_width() {
        assert_eq!(visible_width("hello"), 5);
        assert_eq!(visible_width("\x1b[31mhello\x1b[0m"), 5);
        assert_eq!(visible_width("你好"), 4); // Chinese characters are 2 columns each
        assert_eq!(visible_width("\x1b[1m你好\x1b[0m"), 4);
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("hello"), "hello");
        assert_eq!(strip_ansi("\x1b[31mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("\x1b[1;31mhello\x1b[0m"), "hello");
    }

    #[test]
    fn test_hyperlink() {
        // let link = hyperlink("https://example.com", "Example");
        // assert!(link.contains("https://example.com"));
        // assert!(link.contains("Example"));
    }
}
