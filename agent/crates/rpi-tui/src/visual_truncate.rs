//! Visual-line truncation utility.
//!
//! Port of `packages/coding-agent/src/modes/interactive/components/visual-truncate.ts`.
//! Renders text through a temporary [`Text`] component (which wraps to the
//! given width) and returns the **last** `max_visual_lines` lines, plus a count
//! of how many were skipped. Used by the bash/tool preview to show the tail of
//! streaming or large outputs.

use crate::component::Component;
use crate::text::Text;

/// Result of [`truncate_to_visual_lines`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualTruncateResult {
    /// The visual lines to display.
    pub visual_lines: Vec<String>,
    /// Number of visual lines hidden (everything before the kept tail).
    pub skipped_count: usize,
}

/// Truncate `text` to the last `max_visual_lines` wrapped lines.
///
/// * `text` — raw text (may contain newlines and ANSI escapes)
/// * `max_visual_lines` — how many wrapped lines to keep
/// * `width` — render/wrap width in columns
/// * `padding_x` — horizontal padding forwarded to the temp `Text` (use 0 when
///   the result will live inside a [`crate::box_component::Box`] which adds its
///   own padding; use 1 for a plain container).
pub fn truncate_to_visual_lines(
    text: &str,
    max_visual_lines: usize,
    width: usize,
    padding_x: usize,
) -> VisualTruncateResult {
    if text.is_empty() || max_visual_lines == 0 {
        return VisualTruncateResult {
            visual_lines: Vec::new(),
            skipped_count: 0,
        };
    }

    let temp = Text::new(text, padding_x, 0);
    let all = temp.render(width);

    if all.len() <= max_visual_lines {
        return VisualTruncateResult {
            visual_lines: all,
            skipped_count: 0,
        };
    }

    let skipped_count = all.len() - max_visual_lines;
    let visual_lines = all[all.len() - max_visual_lines..].to_vec();
    VisualTruncateResult {
        visual_lines,
        skipped_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_text() {
        let r = truncate_to_visual_lines("", 5, 40, 1);
        assert!(r.visual_lines.is_empty());
        assert_eq!(r.skipped_count, 0);
    }

    #[test]
    fn test_short_text_kept_full() {
        let r = truncate_to_visual_lines("hello\nworld", 5, 40, 0);
        assert_eq!(r.visual_lines, vec!["hello", "world"]);
        assert_eq!(r.skipped_count, 0);
    }

    #[test]
    fn test_long_text_tail_kept() {
        // 4 lines, keep last 2.
        let text = "a\nb\nc\nd";
        let r = truncate_to_visual_lines(text, 2, 40, 0);
        assert_eq!(r.visual_lines, vec!["c", "d"]);
        assert_eq!(r.skipped_count, 2);
    }

    #[test]
    fn test_multiline_tail_kept() {
        // 6 logical lines, keep last 2. (The Rust Text component truncates
        // each line to `width` rather than wrapping, so "visual lines" here
        // means logical lines width-truncated — the realistic bash-output case.)
        let text = "line1\nline2\nline3\nline4\nline5\nline6";
        let r = truncate_to_visual_lines(text, 2, 40, 0);
        assert_eq!(r.visual_lines.len(), 2);
        assert_eq!(r.skipped_count, 4);
        // First kept line is line5 (the 5th logical line).
        assert!(r.visual_lines[0].contains("line5"));
    }

    #[test]
    fn test_cjk_no_panic() {
        // Wide chars must not panic the width-truncation path.
        let text = "你好\n世界\n你好\n世界";
        let r = truncate_to_visual_lines(text, 2, 6, 0);
        assert!(r.visual_lines.len() <= 2);
    }
}
