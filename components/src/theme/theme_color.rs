use std::sync::Arc;

use crate::{theme::DEFAULT_THEME_COLORS, ThemeMode};

use gpui::Hsla;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub struct ThemeColor {

    pub accent: Hsla,

    pub accent_foreground: Hsla,

    pub accordion: Hsla,

    pub accordion_hover: Hsla,

    pub background: Hsla,

    pub border: Hsla,

    pub group_box: Hsla,

    pub group_box_foreground: Hsla,

    pub caret: Hsla,

    pub chart_1: Hsla,

    pub chart_2: Hsla,

    pub chart_3: Hsla,

    pub chart_4: Hsla,

    pub chart_5: Hsla,

    pub danger: Hsla,

    pub danger_active: Hsla,

    pub danger_foreground: Hsla,

    pub danger_hover: Hsla,

    pub description_list_label: Hsla,

    pub description_list_label_foreground: Hsla,

    pub drag_border: Hsla,

    pub drop_target: Hsla,

    pub foreground: Hsla,

    pub info: Hsla,

    pub info_active: Hsla,

    pub info_foreground: Hsla,

    pub info_hover: Hsla,

    pub input: Hsla,

    pub link: Hsla,

    pub link_active: Hsla,

    pub link_hover: Hsla,

    pub list: Hsla,

    pub list_active: Hsla,

    pub list_active_border: Hsla,

    pub list_even: Hsla,

    pub list_head: Hsla,

    pub list_hover: Hsla,

    pub muted: Hsla,

    pub muted_foreground: Hsla,

    pub popover: Hsla,

    pub popover_foreground: Hsla,

    pub primary: Hsla,

    pub primary_active: Hsla,

    pub primary_foreground: Hsla,

    pub primary_hover: Hsla,

    pub progress_bar: Hsla,

    pub ring: Hsla,

    pub scrollbar: Hsla,

    pub scrollbar_thumb: Hsla,

    pub scrollbar_thumb_hover: Hsla,

    pub secondary: Hsla,

    pub secondary_active: Hsla,

    pub secondary_foreground: Hsla,

    pub secondary_hover: Hsla,

    pub selection: Hsla,

    pub sidebar: Hsla,

    pub sidebar_accent: Hsla,

    pub sidebar_accent_foreground: Hsla,

    pub sidebar_border: Hsla,

    pub sidebar_foreground: Hsla,

    pub sidebar_primary: Hsla,

    pub sidebar_primary_foreground: Hsla,

    pub skeleton: Hsla,

    pub slider_bar: Hsla,

    pub slider_thumb: Hsla,

    pub success: Hsla,

    pub success_foreground: Hsla,

    pub success_hover: Hsla,

    pub success_active: Hsla,

    pub bullish: Hsla,

    pub bearish: Hsla,

    pub switch: Hsla,

    pub switch_thumb: Hsla,

    pub tab: Hsla,

    pub tab_active: Hsla,

    pub tab_active_foreground: Hsla,

    pub tab_bar: Hsla,

    pub tab_bar_segmented: Hsla,

    pub tab_foreground: Hsla,

    pub table: Hsla,

    pub table_active: Hsla,

    pub table_active_border: Hsla,

    pub table_even: Hsla,

    pub table_head: Hsla,

    pub table_head_foreground: Hsla,

    pub table_hover: Hsla,

    pub table_row_border: Hsla,

    pub title_bar: Hsla,

    pub title_bar_border: Hsla,

    pub tiles: Hsla,

    pub warning: Hsla,

    pub warning_active: Hsla,

    pub warning_hover: Hsla,

    pub warning_foreground: Hsla,

    pub overlay: Hsla,

    pub window_border: Hsla,

    pub red: Hsla,

    pub red_light: Hsla,

    pub green: Hsla,

    pub green_light: Hsla,

    pub blue: Hsla,

    pub blue_light: Hsla,

    pub yellow: Hsla,

    pub yellow_light: Hsla,

    pub magenta: Hsla,

    pub magenta_light: Hsla,

    pub cyan: Hsla,

    pub cyan_light: Hsla,
}

impl ThemeColor {

    pub fn light() -> Arc<Self> {
        DEFAULT_THEME_COLORS[&ThemeMode::Light].0.clone()
    }

    pub fn dark() -> Arc<Self> {
        DEFAULT_THEME_COLORS[&ThemeMode::Dark].0.clone()
    }
}
