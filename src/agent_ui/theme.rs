//! Theme bindings for agent cards. Surfaces and controls follow Kathamo's
//! resolved tokens, including the user's theme overrides.
use crate::theme::ThemeTokens;
use gpui::{App, rgba};

/// Keep the agent component library in sync with Kathamo's current palette.
pub(crate) fn sync(tokens: ThemeTokens, cx: &mut App) {
    let colors = &mut gpui_kit::component::Theme::global_mut(cx).colors;
    colors.background = tokens.card_bg;
    colors.foreground = tokens.text_primary;
    colors.border = tokens.border;
    colors.group_box = tokens.card_bg;
    colors.group_box_foreground = tokens.text_primary;
    colors.popover = tokens.card_bg;
    colors.popover_foreground = tokens.text_primary;
    colors.input = tokens.border;
    colors.muted = tokens.card_hover;
    colors.muted_foreground = tokens.text_muted;
    colors.accent = tokens.card_hover;
    colors.accent_foreground = tokens.text_primary;
    colors.primary = tokens.primary_btn_bg;
    colors.primary_foreground = tokens.primary_btn_fg;
    colors.primary_hover = tokens.primary_btn_hover;
    colors.primary_active = tokens.primary_btn_hover;
    colors.button_primary = tokens.primary_btn_bg;
    colors.button_primary_foreground = tokens.primary_btn_fg;
    colors.button_primary_hover = tokens.primary_btn_hover;
    colors.button_primary_active = tokens.primary_btn_hover;
    colors.secondary = tokens.secondary_btn_bg;
    colors.secondary_foreground = tokens.secondary_btn_fg;
    colors.secondary_hover = tokens.secondary_btn_hover;
    colors.secondary_active = tokens.secondary_btn_hover;
    colors.button_secondary = tokens.secondary_btn_bg;
    colors.button_secondary_foreground = tokens.secondary_btn_fg;
    colors.button_secondary_hover = tokens.secondary_btn_hover;
    colors.button_secondary_active = tokens.secondary_btn_hover;
    colors.ring = tokens.ring;
    colors.tab_bar = tokens.card_bg;
    colors.tab = tokens.card_bg;
    colors.tab_foreground = tokens.text_secondary;
    colors.tab_active = tokens.card_hover;
    colors.tab_active_foreground = tokens.text_primary;
    colors.table = tokens.card_bg;
    colors.table_head = tokens.card_hover;
    colors.table_head_foreground = tokens.text_secondary;
    colors.table_row_border = tokens.border_subtle;
    let palette = visual_colors(tokens.is_dark);
    colors.chart_1 = rgba(palette[0]).into();
    colors.chart_2 = rgba(palette[1]).into();
    colors.chart_3 = rgba(palette[2]).into();
    colors.chart_4 = rgba(palette[3]).into();
    colors.chart_5 = rgba(palette[4]).into();
}

/// T3 Code's primary blue, with complementary visualization colors.
pub(super) fn visual_colors(dark: bool) -> [u32; 7] {
    [
        if dark { 0x346bf1ff } else { 0x1b4ed8ff },
        0x047857ff,
        0xb45309ff,
        0x7e22ceff,
        0xb91c1cff,
        0x0e7490ff,
        0x6d28d9ff,
    ]
}
