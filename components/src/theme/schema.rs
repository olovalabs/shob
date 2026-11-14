use std::{rc::Rc, sync::Arc};

use anyhow::Result;
use gpui::{Hsla, SharedString, px};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Colorize, Theme, ThemeColor, ThemeMode,
    highlighter::{HighlightTheme, HighlightThemeStyle},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ThemeSet {

    pub name: SharedString,

    pub author: Option<SharedString>,

    pub url: Option<SharedString>,

    #[serde(rename = "themes")]
    pub themes: Vec<ThemeConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ThemeConfig {

    pub is_default: bool,

    pub name: SharedString,

    pub mode: ThemeMode,

    #[serde(rename = "font.size")]
    pub font_size: Option<f32>,

    #[serde(rename = "font.family")]
    pub font_family: Option<SharedString>,

    #[serde(rename = "mono_font.family")]
    pub mono_font_family: Option<SharedString>,

    #[serde(rename = "mono_font.size")]
    pub mono_font_size: Option<f32>,

    #[serde(rename = "radius")]
    pub radius: Option<usize>,

    #[serde(rename = "radius.lg")]
    pub radius_lg: Option<usize>,

    #[serde(rename = "shadow")]
    pub shadow: Option<bool>,

    pub colors: ThemeConfigColors,

    pub highlight: Option<HighlightThemeStyle>,
}

#[derive(Debug, Default, Clone, JsonSchema, Serialize, Deserialize)]
pub struct ThemeConfigColors {

    #[serde(rename = "accent.background")]
    pub accent: Option<SharedString>,

    #[serde(rename = "accent.foreground")]
    pub accent_foreground: Option<SharedString>,

    #[serde(rename = "accordion.background")]
    pub accordion: Option<SharedString>,

    #[serde(rename = "accordion.hover.background")]
    pub accordion_hover: Option<SharedString>,

    #[serde(rename = "background")]
    pub background: Option<SharedString>,

    #[serde(rename = "border")]
    pub border: Option<SharedString>,

    #[serde(rename = "group_box.background")]
    pub group_box: Option<SharedString>,

    #[serde(rename = "group_box.foreground")]
    pub group_box_foreground: Option<SharedString>,

    #[serde(rename = "group_box.title.foreground")]
    pub group_box_title_foreground: Option<SharedString>,

    #[serde(rename = "caret")]
    pub caret: Option<SharedString>,

    #[serde(rename = "chart.1")]
    pub chart_1: Option<SharedString>,

    #[serde(rename = "chart.2")]
    pub chart_2: Option<SharedString>,

    #[serde(rename = "chart.3")]
    pub chart_3: Option<SharedString>,

    #[serde(rename = "chart.4")]
    pub chart_4: Option<SharedString>,

    #[serde(rename = "chart.5")]
    pub chart_5: Option<SharedString>,

    #[serde(rename = "danger.background")]
    pub danger: Option<SharedString>,

    #[serde(rename = "danger.active.background")]
    pub danger_active: Option<SharedString>,

    #[serde(rename = "danger.foreground")]
    pub danger_foreground: Option<SharedString>,

    #[serde(rename = "danger.hover.background")]
    pub danger_hover: Option<SharedString>,

    #[serde(rename = "description_list.label.background")]
    pub description_list_label: Option<SharedString>,

    #[serde(rename = "description_list.label.foreground")]
    pub description_list_label_foreground: Option<SharedString>,

    #[serde(rename = "drag.border")]
    pub drag_border: Option<SharedString>,

    #[serde(rename = "drop_target.background")]
    pub drop_target: Option<SharedString>,

    #[serde(rename = "foreground")]
    pub foreground: Option<SharedString>,

    #[serde(rename = "info.background")]
    pub info: Option<SharedString>,

    #[serde(rename = "info.active.background")]
    pub info_active: Option<SharedString>,

    #[serde(rename = "info.foreground")]
    pub info_foreground: Option<SharedString>,

    #[serde(rename = "info.hover.background")]
    pub info_hover: Option<SharedString>,

    #[serde(rename = "input.border")]
    pub input: Option<SharedString>,

    #[serde(rename = "link")]
    pub link: Option<SharedString>,

    #[serde(rename = "link.active")]
    pub link_active: Option<SharedString>,

    #[serde(rename = "link.hover")]
    pub link_hover: Option<SharedString>,

    #[serde(rename = "list.background")]
    pub list: Option<SharedString>,

    #[serde(rename = "list.active.background")]
    pub list_active: Option<SharedString>,

    #[serde(rename = "list.active.border")]
    pub list_active_border: Option<SharedString>,

    #[serde(rename = "list.even.background")]
    pub list_even: Option<SharedString>,

    #[serde(rename = "list.head.background")]
    pub list_head: Option<SharedString>,

    #[serde(rename = "list.hover.background")]
    pub list_hover: Option<SharedString>,

    #[serde(rename = "muted.background")]
    pub muted: Option<SharedString>,

    #[serde(rename = "muted.foreground")]
    pub muted_foreground: Option<SharedString>,

    #[serde(rename = "popover.background")]
    pub popover: Option<SharedString>,

    #[serde(rename = "popover.foreground")]
    pub popover_foreground: Option<SharedString>,

    #[serde(rename = "primary.background")]
    pub primary: Option<SharedString>,

    #[serde(rename = "primary.active.background")]
    pub primary_active: Option<SharedString>,

    #[serde(rename = "primary.foreground")]
    pub primary_foreground: Option<SharedString>,

    #[serde(rename = "primary.hover.background")]
    pub primary_hover: Option<SharedString>,

    #[serde(rename = "progress.bar.background")]
    pub progress_bar: Option<SharedString>,

    #[serde(rename = "ring")]
    pub ring: Option<SharedString>,

    #[serde(rename = "scrollbar.background")]
    pub scrollbar: Option<SharedString>,

    #[serde(rename = "scrollbar.thumb.background")]
    pub scrollbar_thumb: Option<SharedString>,

    #[serde(rename = "scrollbar.thumb.hover.background")]
    pub scrollbar_thumb_hover: Option<SharedString>,

    #[serde(rename = "secondary.background")]
    pub secondary: Option<SharedString>,

    #[serde(rename = "secondary.active.background")]
    pub secondary_active: Option<SharedString>,

    #[serde(rename = "secondary.foreground")]
    pub secondary_foreground: Option<SharedString>,

    #[serde(rename = "secondary.hover.background")]
    pub secondary_hover: Option<SharedString>,

    #[serde(rename = "selection.background")]
    pub selection: Option<SharedString>,

    #[serde(rename = "sidebar.background")]
    pub sidebar: Option<SharedString>,

    #[serde(rename = "sidebar.accent.background")]
    pub sidebar_accent: Option<SharedString>,

    #[serde(rename = "sidebar.accent.foreground")]
    pub sidebar_accent_foreground: Option<SharedString>,

    #[serde(rename = "sidebar.border")]
    pub sidebar_border: Option<SharedString>,

    #[serde(rename = "sidebar.foreground")]
    pub sidebar_foreground: Option<SharedString>,

    #[serde(rename = "sidebar.primary.background")]
    pub sidebar_primary: Option<SharedString>,

    #[serde(rename = "sidebar.primary.foreground")]
    pub sidebar_primary_foreground: Option<SharedString>,

    #[serde(rename = "skeleton.background")]
    pub skeleton: Option<SharedString>,

    #[serde(rename = "slider.background")]
    pub slider_bar: Option<SharedString>,

    #[serde(rename = "slider.thumb.background")]
    pub slider_thumb: Option<SharedString>,

    #[serde(rename = "success.background")]
    pub success: Option<SharedString>,

    #[serde(rename = "success.foreground")]
    pub success_foreground: Option<SharedString>,

    #[serde(rename = "success.hover.background")]
    pub success_hover: Option<SharedString>,

    #[serde(rename = "success.active.background")]
    pub success_active: Option<SharedString>,

    #[serde(rename = "bullish.background")]
    pub bullish: Option<SharedString>,

    #[serde(rename = "bearish.background")]
    pub bearish: Option<SharedString>,

    #[serde(rename = "switch.background")]
    pub switch: Option<SharedString>,

    #[serde(rename = "switch.thumb.background")]
    pub switch_thumb: Option<SharedString>,

    #[serde(rename = "tab.background")]
    pub tab: Option<SharedString>,

    #[serde(rename = "tab.active.background")]
    pub tab_active: Option<SharedString>,

    #[serde(rename = "tab.active.foreground")]
    pub tab_active_foreground: Option<SharedString>,

    #[serde(rename = "tab_bar.background")]
    pub tab_bar: Option<SharedString>,

    #[serde(rename = "tab_bar.segmented.background")]
    pub tab_bar_segmented: Option<SharedString>,

    #[serde(rename = "tab.foreground")]
    pub tab_foreground: Option<SharedString>,

    #[serde(rename = "table.background")]
    pub table: Option<SharedString>,

    #[serde(rename = "table.active.background")]
    pub table_active: Option<SharedString>,

    #[serde(rename = "table.active.border")]
    pub table_active_border: Option<SharedString>,

    #[serde(rename = "table.even.background")]
    pub table_even: Option<SharedString>,

    #[serde(rename = "table.head.background")]
    pub table_head: Option<SharedString>,

    #[serde(rename = "table.head.foreground")]
    pub table_head_foreground: Option<SharedString>,

    #[serde(rename = "table.hover.background")]
    pub table_hover: Option<SharedString>,

    #[serde(rename = "table.row.border")]
    pub table_row_border: Option<SharedString>,

    #[serde(rename = "title_bar.background")]
    pub title_bar: Option<SharedString>,

    #[serde(rename = "title_bar.border")]
    pub title_bar_border: Option<SharedString>,

    #[serde(rename = "tiles.background")]
    pub tiles: Option<SharedString>,

    #[serde(rename = "warning.background")]
    pub warning: Option<SharedString>,

    #[serde(rename = "warning.active.background")]
    pub warning_active: Option<SharedString>,

    #[serde(rename = "warning.hover.background")]
    pub warning_hover: Option<SharedString>,

    #[serde(rename = "warning.foreground")]
    pub warning_foreground: Option<SharedString>,

    #[serde(rename = "overlay")]
    pub overlay: Option<SharedString>,

    #[serde(rename = "window.border")]
    pub window_border: Option<SharedString>,

    #[serde(rename = "base.blue")]
    blue: Option<String>,

    #[serde(rename = "base.blue.light")]
    blue_light: Option<String>,

    #[serde(rename = "base.cyan")]
    cyan: Option<String>,

    #[serde(rename = "base.cyan.light")]
    cyan_light: Option<String>,

    #[serde(rename = "base.green")]
    green: Option<String>,

    #[serde(rename = "base.green.light")]
    green_light: Option<String>,

    #[serde(rename = "base.magenta")]
    magenta: Option<String>,
    #[serde(rename = "base.magenta.light")]
    magenta_light: Option<String>,

    #[serde(rename = "base.red")]
    red: Option<String>,

    #[serde(rename = "base.red.light")]
    red_light: Option<String>,

    #[serde(rename = "base.yellow")]
    yellow: Option<String>,

    #[serde(rename = "base.yellow.light")]
    yellow_light: Option<String>,
}

fn try_parse_color(color: &str) -> Result<Hsla> {
    let rgba = gpui::Rgba::try_from(color)?;
    Ok(rgba.into())
}

impl ThemeColor {

    pub(crate) fn apply_config(&mut self, config: &ThemeConfig, default_theme: &ThemeColor) {
        let colors = config.colors.clone();

        macro_rules! apply_color {
            ($config_field:ident) => {
                if let Some(value) = colors.$config_field {
                    if let Ok(color) = try_parse_color(&value) {
                        self.$config_field = color;
                    } else {
                        self.$config_field = default_theme.$config_field;
                    }
                } else {
                    self.$config_field = default_theme.$config_field;
                }
            };

            ($config_field:ident, fallback = $fallback:expr) => {
                if let Some(value) = colors.$config_field {
                    if let Ok(color) = try_parse_color(&value) {
                        self.$config_field = color;
                    }
                } else {
                    self.$config_field = $fallback;
                }
            };
        }

        apply_color!(background);

        apply_color!(red);
        apply_color!(
            red_light,
            fallback = self.background.blend(self.red.opacity(0.8))
        );
        apply_color!(green);
        apply_color!(
            green_light,
            fallback = self.background.blend(self.green.opacity(0.8))
        );
        apply_color!(blue);
        apply_color!(
            blue_light,
            fallback = self.background.blend(self.blue.opacity(0.8))
        );
        apply_color!(magenta);
        apply_color!(
            magenta_light,
            fallback = self.background.blend(self.magenta.opacity(0.8))
        );
        apply_color!(yellow);
        apply_color!(
            yellow_light,
            fallback = self.background.blend(self.yellow.opacity(0.8))
        );
        apply_color!(cyan);
        apply_color!(
            cyan_light,
            fallback = self.background.blend(self.cyan.opacity(0.8))
        );

        apply_color!(border);
        apply_color!(foreground);
        apply_color!(muted);
        apply_color!(
            muted_foreground,
            fallback = self.muted.blend(self.foreground.opacity(0.7))
        );

        let active_darken = if config.mode.is_dark() { 0.2 } else { 0.1 };
        let hover_opacity = 0.9;
        apply_color!(primary);
        apply_color!(primary_foreground, fallback = self.foreground);
        apply_color!(
            primary_hover,
            fallback = self.background.blend(self.primary.opacity(hover_opacity))
        );
        apply_color!(
            primary_active,
            fallback = self.primary.darken(active_darken)
        );
        apply_color!(secondary);
        apply_color!(secondary_foreground, fallback = self.foreground);
        apply_color!(
            secondary_hover,
            fallback = self.background.blend(self.secondary.opacity(hover_opacity))
        );
        apply_color!(
            secondary_active,
            fallback = self.secondary.darken(active_darken)
        );
        apply_color!(success, fallback = self.green);
        apply_color!(success_foreground, fallback = self.primary_foreground);
        apply_color!(
            success_hover,
            fallback = self.background.blend(self.success.opacity(hover_opacity))
        );
        apply_color!(
            success_active,
            fallback = self.success.darken(active_darken)
        );
        apply_color!(bullish, fallback = self.green);
        apply_color!(bearish, fallback = self.red);
        apply_color!(info, fallback = self.cyan);
        apply_color!(info_foreground, fallback = self.primary_foreground);
        apply_color!(
            info_hover,
            fallback = self.background.blend(self.info.opacity(hover_opacity))
        );
        apply_color!(info_active, fallback = self.info.darken(active_darken));
        apply_color!(warning, fallback = self.yellow);
        apply_color!(warning_foreground, fallback = self.primary_foreground);
        apply_color!(
            warning_hover,
            fallback = self.background.blend(self.warning.opacity(0.9))
        );
        apply_color!(
            warning_active,
            fallback = self.background.blend(self.warning.darken(active_darken))
        );

        apply_color!(accent, fallback = self.secondary);
        apply_color!(accent_foreground, fallback = self.foreground);
        apply_color!(accordion, fallback = self.background);
        apply_color!(accordion_hover, fallback = self.accent.opacity(0.8));
        apply_color!(
            group_box,
            fallback = self
                .background
                .blend(
                    self.secondary
                        .opacity(if config.mode.is_dark() { 0.3 } else { 0.4 })
                )
        );
        apply_color!(group_box_foreground, fallback = self.foreground);
        apply_color!(caret, fallback = self.primary);
        apply_color!(chart_1, fallback = self.blue.lighten(0.4));
        apply_color!(chart_2, fallback = self.blue.lighten(0.2));
        apply_color!(chart_3, fallback = self.blue);
        apply_color!(chart_4, fallback = self.blue.darken(0.2));
        apply_color!(chart_5, fallback = self.blue.darken(0.4));
        apply_color!(danger, fallback = self.red);
        apply_color!(danger_active, fallback = self.danger.darken(active_darken));
        apply_color!(danger_foreground, fallback = self.primary_foreground);
        apply_color!(
            danger_hover,
            fallback = self.background.blend(self.danger.opacity(0.9))
        );
        apply_color!(
            description_list_label,
            fallback = self.background.blend(self.border.opacity(0.2))
        );
        apply_color!(
            description_list_label_foreground,
            fallback = self.muted_foreground
        );
        apply_color!(drag_border, fallback = self.primary.opacity(0.65));
        apply_color!(drop_target, fallback = self.primary.opacity(0.2));
        apply_color!(input, fallback = self.border);
        apply_color!(link, fallback = self.primary);
        apply_color!(link_active, fallback = self.link);
        apply_color!(link_hover, fallback = self.link);
        apply_color!(list, fallback = self.background);
        apply_color!(
            list_active,
            fallback = self.background.blend(self.primary.opacity(0.1))
        );
        apply_color!(
            list_active_border,
            fallback = self.background.blend(self.primary.opacity(0.6))
        );
        apply_color!(list_even, fallback = self.list);
        apply_color!(list_head, fallback = self.list);
        apply_color!(list_hover, fallback = self.secondary_hover);
        apply_color!(popover, fallback = self.background);
        apply_color!(popover_foreground, fallback = self.foreground);
        apply_color!(progress_bar, fallback = self.primary);
        apply_color!(ring, fallback = self.blue);
        apply_color!(scrollbar, fallback = self.background);
        apply_color!(scrollbar_thumb, fallback = self.accent);
        apply_color!(scrollbar_thumb_hover, fallback = self.scrollbar_thumb);
        apply_color!(selection, fallback = self.primary);
        apply_color!(sidebar, fallback = self.background);
        apply_color!(sidebar_accent, fallback = self.accent);
        apply_color!(sidebar_accent_foreground, fallback = self.accent_foreground);
        apply_color!(sidebar_border, fallback = self.border);
        apply_color!(sidebar_foreground, fallback = self.foreground);
        apply_color!(sidebar_primary, fallback = self.primary);
        apply_color!(
            sidebar_primary_foreground,
            fallback = self.primary_foreground
        );
        apply_color!(skeleton, fallback = self.secondary);
        apply_color!(slider_bar, fallback = self.primary);
        apply_color!(slider_thumb, fallback = self.primary_foreground);
        apply_color!(switch, fallback = self.secondary);
        apply_color!(switch_thumb, fallback = self.background);
        apply_color!(tab, fallback = self.background);
        apply_color!(tab_active, fallback = self.background);
        apply_color!(tab_active_foreground, fallback = self.foreground);
        apply_color!(tab_bar, fallback = self.background);
        apply_color!(tab_bar_segmented, fallback = self.secondary);
        apply_color!(tab_foreground, fallback = self.foreground);
        apply_color!(table, fallback = self.list);
        apply_color!(table_active, fallback = self.list_active);
        apply_color!(table_active_border, fallback = self.list_active_border);
        apply_color!(table_even, fallback = self.list_even);
        apply_color!(table_head, fallback = self.list_head);
        apply_color!(table_head_foreground, fallback = self.muted_foreground);
        apply_color!(table_hover, fallback = self.list_hover);
        apply_color!(table_row_border, fallback = self.border);
        apply_color!(title_bar, fallback = self.background);
        apply_color!(title_bar_border, fallback = self.border);
        apply_color!(tiles, fallback = self.background);
        apply_color!(overlay);
        apply_color!(window_border, fallback = self.border);

        self.list_active = self.list_active.alpha(self.list_active.a.min(0.2));
        self.table_active = self.table_active.alpha(self.table_active.a.min(0.2));
        self.selection = self.selection.alpha(self.selection.a.min(0.3));
    }
}

impl Theme {

    pub fn apply_config(&mut self, config: &Rc<ThemeConfig>) {
        if config.mode.is_dark() {
            self.dark_theme = config.clone();
        } else {
            self.light_theme = config.clone();
        }
        if let Some(style) = &config.highlight {
            let highlight_theme = Arc::new(HighlightTheme {
                name: config.name.to_string(),
                appearance: config.mode,
                style: style.clone(),
            });
            self.highlight_theme = highlight_theme.clone();
        }

        let default_theme = if config.mode.is_dark() {
            Self::from(ThemeColor::dark().as_ref())
        } else {
            Self::from(ThemeColor::light().as_ref())
        };

        if let Some(font_size) = config.font_size {
            self.font_size = px(font_size);
        } else {
            self.font_size = default_theme.font_size;
        }
        if let Some(font_family) = &config.font_family {
            self.font_family = font_family.clone();
        } else {
            self.font_family = default_theme.font_family.clone();
        }
        if let Some(mono_font_family) = &config.mono_font_family {
            self.mono_font_family = mono_font_family.clone();
        } else {
            self.mono_font_family = default_theme.mono_font_family.clone();
        }
        if let Some(mono_font_size) = config.mono_font_size {
            self.mono_font_size = px(mono_font_size);
        } else {
            self.mono_font_size = default_theme.mono_font_size;
        }
        if let Some(radius) = config.radius {
            self.radius = px(radius as f32);
        } else {
            self.radius = default_theme.radius;
        }
        if let Some(radius_lg) = config.radius_lg {
            self.radius_lg = px(radius_lg as f32);
        } else {
            self.radius_lg = default_theme.radius_lg;
        }
        if let Some(shadow) = config.shadow {
            self.shadow = shadow;
        } else {
            self.shadow = default_theme.shadow;
        }

        self.colors.apply_config(&config, &default_theme.colors);
        self.mode = config.mode;
    }
}

#[cfg(test)]
mod tests {
    use super::try_parse_color;
    use gpui::hsla;

    #[test]
    fn test_try_parse_color() {
        assert_eq!(
            try_parse_color("#F2F200").ok(),
            Some(hsla(0.16666667, 1., 0.4745098, 1.0))
        );
        assert_eq!(
            try_parse_color("#00f21888").ok(),
            Some(hsla(0.34986225, 1.0, 0.4745098, 0.53333336))
        );
    }
}
