//! Dynamic border component — a horizontal rule that stretches to the viewport
//! width.
//!
//! Port of `packages/coding-agent/src/modes/interactive/components/dynamic-border.ts`.
//! The TS version takes a color function; the Rust port takes an optional
//! [`Color`] (None = theme border color) and uses [`Color::fg`] at render time.

use std::any::Any;

use super::component::Component;
use crate::ansi::dim;
use crate::theme::{theme, Color};

/// A single-row horizontal rule (`─` repeated to `width`) in a chosen color.
///
/// `None` resolves to the low-contrast `border_muted` theme token and is also
/// rendered faint, so command separators never compete with the editor border.
/// Explicit colors remain full strength for intentional accent components.
pub struct DynamicBorder {
    color: Option<Color>,
}

impl DynamicBorder {
    /// Create a border using the current theme's border color.
    pub fn new() -> Self {
        Self { color: None }
    }

    /// Create a border with an explicit color.
    pub fn with_color(color: Color) -> Self {
        Self { color: Some(color) }
    }
}

impl Default for DynamicBorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for DynamicBorder {
    fn render(&self, width: usize) -> Vec<String> {
        let rule = "─".repeat(width.max(1));
        let rendered = match self.color {
            Some(color) => color.fg(&rule),
            None => dim(&theme().colors.border_muted.fg(&rule)),
        };
        vec![rendered]
    }

    fn invalidate(&self) {
        // No cached state.
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::visible_width;

    #[test]
    fn test_dynamic_border_renders_one_line() {
        let border = DynamicBorder::new();
        let lines = border.render(40);
        assert_eq!(lines.len(), 1);
        // Visible width is stable and default separators are deliberately faint.
        assert_eq!(visible_width(&lines[0]), 40);
        assert!(lines[0].contains("\x1b[2m"));
    }

    #[test]
    fn test_dynamic_border_zero_width_safe() {
        // Must not panic on a 0-width viewport.
        let border = DynamicBorder::new();
        let lines = border.render(0);
        assert_eq!(lines.len(), 1);
        assert_eq!(visible_width(&lines[0]), 1);
    }

    #[test]
    fn test_dynamic_border_with_color_contains_escape() {
        let border = DynamicBorder::with_color(Color::Ansi256(39));
        let lines = border.render(10);
        assert!(lines[0].contains('\x1b'));
    }
}
