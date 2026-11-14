//! Shared background and insets for bash and generic tool execution panels.

use crate::theme::{Color, ThemeSpacing};
use crate::utils::{apply_background_to_line, truncate_to_width};

pub(crate) struct ToolPanel {
    width: usize,
    padding: String,
    background: Color,
    pub(crate) content_width: usize,
    pub(crate) padding_y: usize,
}

impl ToolPanel {
    pub(crate) fn new(width: usize, spacing: ThemeSpacing, background: Color) -> Option<Self> {
        if width == 0 {
            return None;
        }
        // Preserve one content column, even with large insets on narrow terminals.
        let padding_x = spacing.tool_panel_padding_x.min((width - 1) / 2);
        Some(Self {
            width,
            padding: " ".repeat(padding_x),
            background,
            content_width: width - padding_x * 2,
            padding_y: spacing.tool_panel_padding_y,
        })
    }

    pub(crate) fn line(&self, content: &str) -> String {
        // In particular, a two-column glyph cannot fit in a one-column panel.
        let content = truncate_to_width(content, self.content_width, "");
        apply_background_to_line(&format!("{}{content}", self.padding), self.width, |s| {
            self.background.bg(s)
        })
    }

    pub(crate) fn blank(&self) -> String {
        self.line("")
    }
}
