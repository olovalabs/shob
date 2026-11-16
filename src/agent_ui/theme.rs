//! Monochrome theme bindings for agent cards, derived from Shob's resolved tokens.
use crate::theme::ThemeTokens;
use gpui::{App, rgba};

pub(super) fn monochrome(mut tokens: ThemeTokens) -> ThemeTokens {
    for color in [
        &mut tokens.card_bg,
        &mut tokens.card_hover,
        &mut tokens.border,
        &mut tokens.border_subtle,
        &mut tokens.text_primary,
        &mut tokens.text_secondary,
        &mut tokens.text_muted,
        &mut tokens.sidebar_item_hover,
        &mut tokens.block_bg,
    ] {
        color.s = 0.0;
    }
    tokens.primary_btn_bg = tokens.text_primary;
    tokens.primary_btn_fg = tokens.card_bg;
    tokens.primary_btn_hover = tokens.text_secondary;
    tokens.secondary_btn_bg = tokens.card_hover;
    tokens.secondary_btn_fg = tokens.text_primary;
    tokens.secondary_btn_hover = tokens.sidebar_item_hover;
    tokens.ring = tokens.text_muted;
    tokens.block_success = tokens.text_primary;
    tokens.block_failure = tokens.text_secondary;
    tokens
}

/// Keep the agent component library in sync with Shob's current palette.
pub(crate) fn sync(tokens: ThemeTokens, cx: &mut App) {
    let tokens = monochrome(tokens);
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
    macro_rules! neutral_tone {
        ($base:ident, $foreground:ident, $hover:ident, $active:ident,
         $button:ident, $button_foreground:ident, $button_hover:ident, $button_active:ident) => {
            colors.$base = tokens.text_primary;
            colors.$foreground = tokens.card_bg;
            colors.$hover = tokens.text_secondary;
            colors.$active = tokens.text_secondary;
            colors.$button = tokens.card_hover;
            colors.$button_foreground = tokens.text_primary;
            colors.$button_hover = tokens.sidebar_item_hover;
            colors.$button_active = tokens.sidebar_item_hover;
        };
    }
    neutral_tone!(
        success,
        success_foreground,
        success_hover,
        success_active,
        button_success,
        button_success_foreground,
        button_success_hover,
        button_success_active
    );
    neutral_tone!(
        warning,
        warning_foreground,
        warning_hover,
        warning_active,
        button_warning,
        button_warning_foreground,
        button_warning_hover,
        button_warning_active
    );
    neutral_tone!(
        danger,
        danger_foreground,
        danger_hover,
        danger_active,
        button_danger,
        button_danger_foreground,
        button_danger_hover,
        button_danger_active
    );
    neutral_tone!(
        info,
        info_foreground,
        info_hover,
        info_active,
        button_info,
        button_info_foreground,
        button_info_hover,
        button_info_active
    );
    colors.progress_bar = tokens.text_primary;
    colors.link = tokens.text_primary;
    colors.link_hover = tokens.text_secondary;
    colors.link_active = tokens.text_secondary;
    let palette = visual_colors(tokens.is_dark);
    colors.chart_1 = rgba(palette[0]).into();
    colors.chart_2 = rgba(palette[1]).into();
    colors.chart_3 = rgba(palette[2]).into();
    colors.chart_4 = rgba(palette[3]).into();
    colors.chart_5 = rgba(palette[4]).into();
}

pub(super) fn visual_colors(dark: bool) -> [u32; 7] {
    if dark {
        [
            0xe5e5e5ff, 0xbfbfbfff, 0x999999ff, 0x737373ff, 0x595959ff, 0x404040ff, 0x262626ff,
        ]
    } else {
        [
            0x262626ff, 0x404040ff, 0x595959ff, 0x737373ff, 0x999999ff, 0xbfbfbfff, 0xe5e5e5ff,
        ]
    }
}
