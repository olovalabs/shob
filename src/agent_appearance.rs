use gpui::{Hsla, Pixels, px};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::{AgentMarkdown, MarkdownColors, TextViewStyle};

pub const COLUMN_WIDTH: Pixels = px(736. + 32.);
pub const GUTTER: Pixels = px(16.);
pub const FONT_SIZE: Pixels = px(14.);
pub const LINE_HEIGHT: Pixels = px(23.);

#[derive(Clone, Copy, Debug)]
pub struct ChatAppearance {
    pub background: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    pub border: Hsla,
    pub surface: Hsla,
    pub composer: Hsla,
    pub user_background: Hsla,
    pub user_foreground: Hsla,
    pub user_border: Hsla,
    pub link: Hsla,
    pub inline_code: Hsla,
}

impl ChatAppearance {
    pub fn from_theme(theme: &crate::theme::ThemeTokens) -> Self {
        Self {
            background: theme.pane_bg,
            foreground: theme.text_primary,
            muted: theme.text_muted,
            border: theme.border_subtle,
            surface: theme.card_bg,
            composer: theme.editor_bar_bg,
            user_background: theme.chat_user_bg,
            user_foreground: theme.chat_user_fg,
            user_border: theme.chat_user_border,
            link: theme.ring,
            inline_code: theme.card_hover,
        }
    }

    pub fn markdown(
        self,
        dark: bool,
        user: bool,
        highlight_theme: std::sync::Arc<HighlightTheme>,
    ) -> TextViewStyle {
        TextViewStyle {
            agent_markdown: if user {
                AgentMarkdown::User
            } else {
                AgentMarkdown::Assistant
            },
            heading_base_font_size: FONT_SIZE,
            paragraph_gap: gpui::rems(1.0),
            is_dark: dark,
            colors: Some(MarkdownColors {
                text_base: if user {
                    self.user_foreground
                } else {
                    self.foreground
                },
                text_muted: self.muted,
                link: self.link,
                border_base: self.border,
                border_muted: self.border,
                code_bg: self.surface,
                shell_code_bg: self.surface,
                inline_code_bg: self.inline_code,
                table_frame_bg: self.surface,
                table_body_bg: self.background,
                table_body_border: self.border,
            }),
            highlight_theme,
            ..Default::default()
        }
        .heading_font_size(|level, _| {
            px(match level {
                1 => 20.,
                2 => 18.,
                3 => 16.,
                _ => 14.,
            })
        })
    }
}

pub fn worked_label(millis: u64) -> String {
    let seconds = millis / 1000;
    if seconds >= 3600 {
        format!("Worked for {}h {}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("Worked for {}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("Worked for {seconds}s")
    }
}
