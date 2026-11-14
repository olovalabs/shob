//! Utilities for TUI rendering.
//!
//! Provides functions for text manipulation with ANSI awareness.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The widest display unit (grapheme cluster) in `text`, in cells.
///
/// A column narrower than this cannot be rendered: a two-cell glyph (CJK,
/// emoji) never fits in a single cell, so `wrap_text_with_ansi` would emit a
/// two-cell line inside a one-cell column and break the grid. The unit is a
/// grapheme cluster, not a scalar: `👩‍💻` is one two-cell unit, while measuring
/// its scalars would score `👩` and `💻` separately and could size the column
/// for half of a sequence that cannot be split.
pub fn widest_char_width(text: &str) -> usize {
    let per_cluster = text
        .graphemes(true)
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(1);
    // Keep the per-scalar fallback for text the cluster model under-measures.
    let per_scalar = text
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .max()
        .unwrap_or(1);
    per_cluster.max(per_scalar).max(1)
}

/// Get the visible width of a string, ignoring ANSI escape sequences.
pub fn visible_width(s: &str) -> usize {
    if s.contains("\x1b_") || s.contains("\x1b]") {
        return crate::ansi::visible_width(s);
    }
    let mut width = 0;
    let mut in_escape = false;
    let mut chars = s.chars().peekable();
    // The visible text between escape sequences is measured as one span.
    // `UnicodeWidthStr` is not additive across grapheme boundaries: ZWJ and
    // VS16 sequences, and script ligatures such as Arabic Lam-Alef, are
    // narrower (or wider) as a whole than as a sum of their scalars, so
    // summing per scalar mis-measured the string it was sizing.
    let mut span = String::new();

    while let Some(c) = chars.next() {
        if in_escape {
            // Check for OSC sequences (ends with BEL or ST)
            if c == '\x07' {
                in_escape = false;
            } else if c == '\\' && chars.peek() == Some(&'\\') {
                chars.next();
                in_escape = false;
            }
            // CSI sequences end with a byte in 0x40-0x7E
            else if ('@'..='~').contains(&c) && c != '[' {
                in_escape = false;
            }
            // OSC ] ... (ends with BEL or ST)
            else if c == ']' {
                // Skip until BEL or ST
                while let Some(&next) = chars.peek() {
                    if next == '\x07' {
                        chars.next();
                        in_escape = false;
                        break;
                    } else if next == '\x1b' {
                        chars.next();
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                            in_escape = false;
                            break;
                        }
                    } else {
                        chars.next();
                    }
                }
            }
            continue;
        }

        if c == '\x1b' {
            in_escape = true;
            width += UnicodeWidthStr::width(span.as_str());
            span.clear();
            continue;
        }

        // Skip other control characters
        if c < ' ' || c == '\x7f' {
            continue;
        }

        span.push(c);
    }

    width + UnicodeWidthStr::width(span.as_str())
}

/// Slice a string by column positions, accounting for ANSI codes.
/// Returns the portion of the string that fits within start_col..end_col.
pub fn slice_by_column(s: &str, start_col: usize, max_cols: usize, pad: bool) -> String {
    let mut result = String::new();
    let mut col = 0;
    let mut in_escape = false;
    let mut pending_styles = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if in_escape {
            pending_styles.push(c);

            // Check for sequence end
            if c == '\x07' {
                in_escape = false;
            } else if c == '\\' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                pending_styles.push('\\');
                i += 1;
                in_escape = false;
            } else if ('@'..='~').contains(&c) && c != '[' {
                in_escape = false;
            }
            // OSC sequence
            else if c == ']' {
                while i + 1 < chars.len() {
                    i += 1;
                    let next = chars[i];
                    pending_styles.push(next);
                    if next == '\x07' {
                        in_escape = false;
                        break;
                    } else if next == '\x1b' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                        i += 1;
                        pending_styles.push('\\');
                        in_escape = false;
                        break;
                    }
                }
            }
            i += 1;
            continue;
        }

        if c == '\x1b' {
            in_escape = true;
            pending_styles.push(c);
            i += 1;
            continue;
        }

        // Skip control characters
        if c < ' ' || c == '\x7f' {
            i += 1;
            continue;
        }

        // Get grapheme width
        let char_width = UnicodeWidthStr::width(c.to_string().as_str());

        // Check if we're at or past start_col
        if col >= start_col && col < start_col + max_cols {
            // Add pending styles before the character
            if !pending_styles.is_empty() {
                result.push_str(&pending_styles);
                pending_styles.clear();
            }
            result.push(c);
        }

        col += char_width;
        i += 1;
    }

    // Pad if needed
    if pad && col < start_col + max_cols {
        let padding = start_col + max_cols - col;
        for _ in 0..padding {
            result.push(' ');
        }
    }

    result
}

/// Truncate a string to fit within a maximum width.
/// Returns the truncated string with optional suffix.
pub fn truncate_to_width(s: &str, max_width: usize, suffix: &str) -> String {
    if max_width == 0 {
        return String::new();
    }

    let width = visible_width(s);
    if width <= max_width {
        return s.to_string();
    }

    let suffix_width = visible_width(suffix);
    let target_width = max_width.saturating_sub(suffix_width);

    if target_width == 0 {
        return suffix.to_string();
    }

    // Check if input has ANSI codes
    let has_ansi = s.contains("\x1b");

    let mut result = String::new();
    let mut current_width = 0;
    let mut in_escape = false;
    let mut style_stack = String::new();

    for c in s.chars() {
        if in_escape {
            style_stack.push(c);
            if c == '\x07' || (('@'..='~').contains(&c) && c != '[') {
                in_escape = false;
            }
            continue;
        }

        if c == '\x1b' {
            in_escape = true;
            style_stack.push(c);
            continue;
        }

        if c < ' ' || c == '\x7f' {
            continue;
        }

        let char_width = UnicodeWidthStr::width(c.to_string().as_str());
        if current_width + char_width > target_width {
            break;
        }

        if !style_stack.is_empty() {
            result.push_str(&style_stack);
            style_stack.clear();
        }
        result.push(c);
        current_width += char_width;
    }

    // Add suffix
    result.push_str(suffix);

    // Only reset style if input had ANSI codes
    if has_ansi {
        result.push_str("\x1b[0m");
    }
    result
}

/// Strip ANSI escape sequences from a string.
pub fn strip_ansi(s: &str) -> String {
    if s.contains("\x1b_") || s.contains("\x1b]") {
        return crate::ansi::strip_ansi(s);
    }
    let mut result = String::new();
    let mut in_escape = false;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if in_escape {
            // OSC ends with BEL or ST
            if c == '\x07' {
                in_escape = false;
            } else if c == '\\' && chars.peek() == Some(&'\\') {
                chars.next();
                in_escape = false;
            }
            // CSI ends with byte in 0x40-0x7E
            else if ('@'..='~').contains(&c) && c != '[' {
                in_escape = false;
            }
            // OSC
            else if c == ']' {
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == '\x07' {
                        break;
                    } else if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
                in_escape = false;
            }
            continue;
        }

        if c == '\x1b' {
            in_escape = true;
            continue;
        }

        result.push(c);
    }

    result
}

/// Wrap text to fit within a maximum width, preserving ANSI codes.
///
/// Iteration is per grapheme cluster, not per scalar: a two-column sequence
/// such as `👩‍💻` (ZWJ) or `👍🏽` (skin-tone modifier) is one display unit and
/// must never be split across rows. Measuring its scalars individually gives
/// 2 + 0 + 2, which used to emit `👩‍` on one row and `💻` on the next.
pub fn wrap_text_with_ansi(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;
    let mut in_escape = false;
    let mut pending_style = String::new();

    for grapheme in s.graphemes(true) {
        // CRLF segments as one grapheme but is a line break like a bare \n
        // (a lone \r keeps the pre-existing contract: dropped as a control).
        if grapheme == "\r\n" || grapheme == "\n" {
            if !pending_style.is_empty() {
                current_line.push_str(&pending_style);
                pending_style.clear();
            }
            lines.push(current_line);
            current_line = String::new();
            current_width = 0;
            continue;
        }

        // ESC starts its own cluster (controls segment apart), so the control
        // filter must never swallow it; `in_escape` means the tail of a
        // sequence is spread over the following clusters.
        if !grapheme.starts_with('\x1b')
            && !in_escape
            && grapheme.chars().all(|c| c < ' ' || c == '\x7f')
        {
            continue;
        }

        // Split the cluster into ANSI control sequences and the printable
        // remainder. A combining mark can share a cluster with an SGR
        // terminator (`m` + U+0301): consuming the whole cluster at the `m`
        // discarded the visible remainder, losing the accent.
        let mut rest = grapheme;
        loop {
            if in_escape {
                let mut consumed = 0;
                for c in rest.chars() {
                    consumed += c.len_utf8();
                    pending_style.push(c);
                    if c == '\x07' || (('@'..='~').contains(&c) && c != '[') {
                        in_escape = false;
                        break;
                    }
                }
                rest = &rest[consumed..];
                if in_escape {
                    // The sequence continues in the next cluster.
                    break;
                }
            }
            if rest.starts_with('\x1b') {
                in_escape = true;
                pending_style.push('\x1b');
                rest = &rest[1..];
                continue;
            }
            if rest.is_empty() {
                break;
            }
            // Printable remainder (zero-width marks included): it is the tail
            // of one display unit and is never split further.
            let cluster_width = UnicodeWidthStr::width(rest);
            if current_width + cluster_width > max_width && current_width > 0 && cluster_width > 0 {
                // Start a new line
                if !pending_style.is_empty() {
                    current_line.push_str(&pending_style);
                }
                lines.push(current_line);
                current_line = String::new();
                current_width = 0;
            }
            if !pending_style.is_empty() {
                current_line.push_str(&pending_style);
                pending_style.clear();
            }
            current_line.push_str(rest);
            current_width += cluster_width;
            break;
        }
    }

    if !current_line.is_empty() {
        if !pending_style.is_empty() {
            current_line.push_str(&pending_style);
        }
        lines.push(current_line);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    if s.contains('\x1b') {
        crate::line_diff::isolate_wrapped_lines(&mut lines);
    }
    lines
}

/// Get the grapheme segmenter for Unicode-aware text handling.
pub fn get_grapheme_segmenter() -> GraphemeSegmenter {
    GraphemeSegmenter
}

/// Grapheme segmenter wrapper.
pub struct GraphemeSegmenter;

impl GraphemeSegmenter {
    /// Segment a string into graphemes.
    pub fn segment<'a>(&'a self, s: &'a str) -> impl Iterator<Item = Grapheme<'a>> + 'a {
        s.graphemes(true).enumerate().map(|(idx, g)| Grapheme {
            segment: g,
            index: idx,
        })
    }
}

/// A grapheme cluster.
#[derive(Debug, Clone)]
pub struct Grapheme<'a> {
    pub segment: &'a str,
    pub index: usize,
}

/// Check if a character is whitespace.
pub fn is_whitespace_char(c: char) -> bool {
    c.is_whitespace() || c == '\t' || c == '\n' || c == '\r'
}

/// Get an OSC 8 hyperlink at a specific column position.
pub fn get_osc8_link_at_column(s: &str, column: usize) -> Option<String> {
    let mut col = 0;
    let mut in_escape = false;
    let mut in_osc8 = false;
    let mut osc8_link = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if in_escape {
            if c == ']' {
                // OSC start - check for OSC 8
                let mut osc_content = String::new();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == '\x07' {
                        break;
                    } else if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                    osc_content.push(next);
                }
                // OSC 8 format: 8;params;uri
                if osc_content.starts_with("8;") {
                    in_osc8 = true;
                    if let Some(uri_start) = osc_content.find(';').map(|i| i + 1) {
                        if uri_start < osc_content.len() {
                            osc8_link = osc_content[uri_start..].to_string();
                        }
                    }
                }
                in_escape = false;
                continue;
            } else if ('@'..='~').contains(&c) && c != '[' {
                in_escape = false;
            }
            continue;
        }

        if c == '\x1b' {
            in_escape = true;
            continue;
        }

        if c < ' ' || c == '\x7f' {
            continue;
        }

        if col == column && in_osc8 && !osc8_link.is_empty() {
            return Some(osc8_link);
        }

        col += UnicodeWidthStr::width(c.to_string().as_str());
    }

    None
}

/// Apply background color to a line, respecting existing styling.
pub fn apply_background_to_line(
    line: &str,
    width: usize,
    bg_fn: impl Fn(&str) -> String,
) -> String {
    let line_width = visible_width(line);
    let padding = " ".repeat(width.saturating_sub(line_width));
    let padded = format!("{}{}", line, padding);
    bg_fn(&padded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_visible_width() {
        assert_eq!(visible_width("Hello"), 5);
        assert_eq!(visible_width("\x1b[31mHello\x1b[0m"), 5);
        assert_eq!(visible_width("你好"), 4); // Chinese characters are wide
    }

    #[test]
    fn test_visible_width_measures_sequences_as_whole_spans() {
        // Each of these is one display unit of two columns. Summing the
        // scalars instead gave 4 for the ZWJ and skin-tone sequences and 1 for
        // the VS16 sequence, so callers sizing a column mis-measured the text.
        for seq in ["👩‍💻", "👍🏽", "❤️"] {
            assert_eq!(visible_width(seq), 2, "{seq:?} is a 2-column unit");
        }
        // A script ligature is narrower as a whole than as a sum of graphemes.
        assert_eq!(visible_width("لا"), 1);
        // The span restarts at an escape sequence, so styles do not merge the
        // text on either side of them into one measurement.
        assert_eq!(visible_width("\x1b[1m👩‍💻\x1b[0m"), 2);
        assert_eq!(visible_width("a\x1b[1m👩‍💻\x1b[0mb"), 4);
    }

    #[test]
    fn test_widest_char_width_uses_grapheme_clusters() {
        // The floor for a table column is the widest indivisible unit. `❤️` is
        // a 2-column unit whose widest scalar (U+2764) measures 1, so a column
        // floored on scalars would under-reserve it and the sequence would
        // spill past the cell border.
        assert_eq!(widest_char_width("❤️"), 2);
        assert_eq!(widest_char_width("👩‍💻"), 2);
        // A one-column ligature must not be inflated to two.
        assert_eq!(widest_char_width("لا"), 1);
        assert_eq!(widest_char_width("你好"), 2);
        assert_eq!(widest_char_width("abc"), 1);
        // A column can never be zero cells wide.
        assert_eq!(widest_char_width("\u{200b}"), 1);
    }

    #[test]
    fn test_wrap_text_never_splits_a_sequence() {
        for seq in ["👩‍💻", "👍🏽", "❤️"] {
            // Width 2 is the exact cluster width: it must fit on one line.
            let lines = wrap_text_with_ansi(&format!("a{seq}b"), 3);
            assert!(
                lines.iter().any(|l| l.contains(seq)),
                "{seq:?} was split: {lines:?}"
            );
            assert!(
                lines.iter().all(|l| visible_width(l) <= 3),
                "{seq:?} overflowed the wrap width: {lines:?}"
            );
        }
    }

    #[test]
    fn wrap_preserves_crlf_line_breaks() {
        // CRLF segments as one grapheme; dropping it as controls glued the
        // rows together ("firstsecond"), losing the newline contract the
        // per-char wrapper had.
        assert_eq!(
            wrap_text_with_ansi("first\r\nsecond", 80),
            ["first", "second"]
        );
        // A bare \r keeps the existing contract: dropped as a control.
        assert_eq!(wrap_text_with_ansi("first\rsecond", 80), ["firstsecond"]);
    }

    #[test]
    fn wrap_keeps_visible_tail_after_an_ansi_sequence_inside_a_cluster() {
        // A combining mark can share a cluster with the SGR terminator (`m` +
        // U+0301); consuming the whole cluster at the `m` lost the accent.
        let lines = wrap_text_with_ansi("a\x1b[31m\u{301}b", 80);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains('\u{301}'),
            "combining mark after SGR was dropped: {lines:?}"
        );
        // A sequence spanning clusters still terminates correctly.
        assert_eq!(
            strip_ansi(&wrap_text_with_ansi("\x1b[31mred\x1b[0m", 80).join("")),
            "red"
        );
    }

    #[test]
    fn test_truncate_to_width() {
        assert_eq!(truncate_to_width("Hello World", 5, ""), "Hello");
        assert_eq!(truncate_to_width("Hello World", 8, "..."), "Hello...");
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\x1b[31mHello\x1b[0m"), "Hello");
        assert_eq!(strip_ansi("\x1b[1;31mWorld\x1b[0m"), "World");
    }

    #[test]
    fn test_wrap_text() {
        let lines = wrap_text_with_ansi("Hello World", 5);
        assert_eq!(lines, vec!["Hello", " Worl", "d"]);
    }
}
