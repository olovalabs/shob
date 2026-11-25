use gpui::{App, Hsla, Window, rgba};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use terminal::ColorPalette;

/// Apply one appearance to the app chrome and generated chat components.
pub(crate) fn apply_component_theme(
    mode: gpui_component::ThemeMode,
    window: Option<&mut Window>,
    cx: &mut App,
) {
    gpui_component::Theme::change(mode, window, cx);
    let kit_mode = if mode.is_dark() {
        gpui_kit::component::ThemeMode::Dark
    } else {
        gpui_kit::component::ThemeMode::Light
    };
    gpui_kit::component::Theme::change(kit_mode, None, cx);
}

/// Which terminal color palette / block chrome to use.
///
/// `Warp` reproduces the default theme of the Warp terminal (near-black
/// background, white foreground, cyan accent and Warp's ANSI palette) and is
/// the default. `GitHub` keeps the previous GitHub Dark / Light palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaletteScheme {
    #[default]
    Warp,
    Github,
    MidnightGarden,
}

impl PaletteScheme {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Warp => "Warp",
            Self::Github => "GitHub",
            Self::MidnightGarden => "Gradient ASCII",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    #[allow(dead_code)]
    pub fn description(&self) -> &'static str {
        match self {
            Self::System => "Automatically match system appearance",
            Self::Light => "Clean, crisp light theme",
            Self::Dark => "Sleek high-contrast dark theme",
        }
    }

    pub fn resolve_mode(&self, appearance: gpui::WindowAppearance) -> gpui_component::ThemeMode {
        match self {
            Self::System => appearance.into(),
            Self::Light => gpui_component::ThemeMode::Light,
            Self::Dark => gpui_component::ThemeMode::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct ThemeTokens {
    pub is_dark: bool,

    // Window & background
    pub window_bg: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_muted: Hsla,
    pub border: Hsla,
    pub border_subtle: Hsla,

    // Titlebar
    pub titlebar_bg: Hsla,
    pub titlebar_border: Hsla,
    pub titlebar_fg: Hsla,
    pub titlebar_btn_hover: Hsla,

    // Sidebar
    pub sidebar_bg: Hsla,
    pub sidebar_bg_start: Hsla,
    pub sidebar_bg_end: Hsla,
    pub sidebar_border: Hsla,
    pub sidebar_fg: Hsla,
    pub sidebar_muted: Hsla,
    pub sidebar_item_hover: Hsla,
    pub sidebar_item_active: Hsla,
    pub sidebar_item_active_fg: Hsla,
    pub sidebar_section_header: Hsla,
    pub sidebar_icon_btn_hover: Hsla,
    pub sidebar_icon: Hsla,
    pub sidebar_icon_hover: Hsla,
    pub sidebar_footer_bg: Hsla,
    pub sidebar_footer_border: Hsla,

    // Terminal Panes
    pub pane_bg: Hsla,
    pub pane_border: Hsla,
    pub pane_border_focused: Hsla,
    pub pane_header_bg: Hsla,
    pub pane_header_fg: Hsla,
    pub pane_header_btn_hover: Hsla,
    pub pane_header_icon: Hsla,
    pub pane_header_icon_hover: Hsla,

    // Controls & Buttons
    pub primary_btn_bg: Hsla,
    pub primary_btn_hover: Hsla,
    pub primary_btn_fg: Hsla,
    pub secondary_btn_bg: Hsla,
    pub secondary_btn_hover: Hsla,
    pub secondary_btn_fg: Hsla,

    // Cards & Panels (Settings page & Dialogs)
    pub card_bg: Hsla,
    pub card_border: Hsla,
    pub card_hover: Hsla,
    pub model_picker_bg: Hsla,
    pub model_picker_accent: Hsla,
    pub pill_active_bg: Hsla,
    pub pill_active_fg: Hsla,
    pub pill_inactive_bg: Hsla,
    pub pill_inactive_fg: Hsla,
    pub ring: Hsla,

    // Command blocks (Warp style)
    pub block_bg: Hsla,
    pub block_bg_hover: Hsla,
    pub block_border: Hsla,
    pub block_border_selected: Hsla,
    pub block_success: Hsla,
    pub block_failure: Hsla,
    pub block_warning: Hsla,
    pub block_separator: Hsla,
    pub block_status_bg: Hsla,
    pub editor_bar_bg: Hsla,
    pub editor_bar_border: Hsla,

    // Chat user prompt bubble
    pub chat_user_bg: Hsla,
    pub chat_user_fg: Hsla,
    pub chat_user_border: Hsla,
}

/// Per-appearance overrides. Colors use #RRGGBB or #RRGGBBAA.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeColors {
    pub light: AppearanceColors,
    pub dark: AppearanceColors,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceColors {
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub ui: std::collections::BTreeMap<String, String>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub terminal: std::collections::BTreeMap<String, String>,
    pub style: std::collections::BTreeMap<String, String>,
}

impl ThemeColors {
    /// A complete editable copy of Shob's built-in Warp colors.
    pub fn complete_defaults(scheme: PaletteScheme) -> Self {
        fn hex(color: Hsla) -> String {
            let rgb = terminal::hsla_to_rgb(color);
            format!(
                "#{:02x}{:02x}{:02x}{:02x}",
                rgb.r,
                rgb.g,
                rgb.b,
                (color.a.clamp(0.0, 1.0) * 255.0).round() as u8
            )
        }
        fn appearance(dark: bool, scheme: PaletteScheme) -> AppearanceColors {
            let mode = if dark {
                gpui_component::ThemeMode::Dark
            } else {
                gpui_component::ThemeMode::Light
            };
            let tokens = ThemeTokens::from_mode_with_scheme(mode, scheme);
            let palette = load_palette(scheme, dark);
            let mut style = std::collections::BTreeMap::new();
            style.insert("window.bg".into(), hex(tokens.window_bg));
            style.insert("text.primary".into(), hex(tokens.text_primary));
            style.insert("text.secondary".into(), hex(tokens.text_secondary));
            style.insert("text.muted".into(), hex(tokens.text_muted));
            style.insert("border".into(), hex(tokens.border));
            style.insert("border.subtle".into(), hex(tokens.border_subtle));
            style.insert("titlebar.bg".into(), hex(tokens.titlebar_bg));
            style.insert("titlebar.border".into(), hex(tokens.titlebar_border));
            style.insert("titlebar.fg".into(), hex(tokens.titlebar_fg));
            style.insert("titlebar.btn.hover".into(), hex(tokens.titlebar_btn_hover));
            style.insert("sidebar.bg".into(), hex(tokens.sidebar_bg));
            style.insert("sidebar.bg.start".into(), hex(tokens.sidebar_bg_start));
            style.insert("sidebar.bg.end".into(), hex(tokens.sidebar_bg_end));
            style.insert("sidebar.border".into(), hex(tokens.sidebar_border));
            style.insert("sidebar.fg".into(), hex(tokens.sidebar_fg));
            style.insert("sidebar.muted".into(), hex(tokens.sidebar_muted));
            style.insert("sidebar.item.hover".into(), hex(tokens.sidebar_item_hover));
            style.insert(
                "sidebar.item.active".into(),
                hex(tokens.sidebar_item_active),
            );
            style.insert(
                "sidebar.item.active.fg".into(),
                hex(tokens.sidebar_item_active_fg),
            );
            style.insert(
                "sidebar.section.header".into(),
                hex(tokens.sidebar_section_header),
            );
            style.insert(
                "sidebar.icon.btn.hover".into(),
                hex(tokens.sidebar_icon_btn_hover),
            );
            style.insert("sidebar.icon".into(), hex(tokens.sidebar_icon));
            style.insert("sidebar.icon.hover".into(), hex(tokens.sidebar_icon_hover));
            style.insert("sidebar.footer.bg".into(), hex(tokens.sidebar_footer_bg));
            style.insert(
                "sidebar.footer.border".into(),
                hex(tokens.sidebar_footer_border),
            );
            style.insert("pane.bg".into(), hex(tokens.pane_bg));
            style.insert("pane.border".into(), hex(tokens.pane_border));
            style.insert(
                "pane.border.focused".into(),
                hex(tokens.pane_border_focused),
            );
            style.insert("pane.header.bg".into(), hex(tokens.pane_header_bg));
            style.insert("pane.header.fg".into(), hex(tokens.pane_header_fg));
            style.insert(
                "pane.header.btn.hover".into(),
                hex(tokens.pane_header_btn_hover),
            );
            style.insert("pane.header.icon".into(), hex(tokens.pane_header_icon));
            style.insert(
                "pane.header.icon.hover".into(),
                hex(tokens.pane_header_icon_hover),
            );
            style.insert("primary.btn.bg".into(), hex(tokens.primary_btn_bg));
            style.insert("primary.btn.hover".into(), hex(tokens.primary_btn_hover));
            style.insert("primary.btn.fg".into(), hex(tokens.primary_btn_fg));
            style.insert("secondary.btn.bg".into(), hex(tokens.secondary_btn_bg));
            style.insert(
                "secondary.btn.hover".into(),
                hex(tokens.secondary_btn_hover),
            );
            style.insert("secondary.btn.fg".into(), hex(tokens.secondary_btn_fg));
            style.insert("card.bg".into(), hex(tokens.card_bg));
            style.insert("card.border".into(), hex(tokens.card_border));
            style.insert("card.hover".into(), hex(tokens.card_hover));
            style.insert("model_picker.bg".into(), hex(tokens.model_picker_bg));
            style.insert(
                "model_picker.accent".into(),
                hex(tokens.model_picker_accent),
            );
            style.insert("pill.active.bg".into(), hex(tokens.pill_active_bg));
            style.insert("pill.active.fg".into(), hex(tokens.pill_active_fg));
            style.insert("pill.inactive.bg".into(), hex(tokens.pill_inactive_bg));
            style.insert("pill.inactive.fg".into(), hex(tokens.pill_inactive_fg));
            style.insert("ring".into(), hex(tokens.ring));
            style.insert("block.bg".into(), hex(tokens.block_bg));
            style.insert("block.bg.hover".into(), hex(tokens.block_bg_hover));
            style.insert("block.border".into(), hex(tokens.block_border));
            style.insert(
                "block.border.selected".into(),
                hex(tokens.block_border_selected),
            );
            style.insert("block.success".into(), hex(tokens.block_success));
            style.insert("block.failure".into(), hex(tokens.block_failure));
            style.insert("block.warning".into(), hex(tokens.block_warning));
            style.insert("block.separator".into(), hex(tokens.block_separator));
            style.insert("block.status.bg".into(), hex(tokens.block_status_bg));
            style.insert("editor.bar.bg".into(), hex(tokens.editor_bar_bg));
            style.insert("editor.bar.border".into(), hex(tokens.editor_bar_border));
            style.insert("chat.user.bg".into(), hex(tokens.chat_user_bg));
            style.insert("chat.user.fg".into(), hex(tokens.chat_user_fg));
            style.insert("chat.user.border".into(), hex(tokens.chat_user_border));
            style.insert("terminal.background".into(), hex(palette.background()));
            style.insert("terminal.foreground".into(), hex(palette.foreground()));
            style.insert("terminal.cursor".into(), hex(palette.cursor()));
            style.insert("terminal.ansi.black".into(), hex(palette.ansi_colors()[0]));
            style.insert("terminal.ansi.red".into(), hex(palette.ansi_colors()[1]));
            style.insert("terminal.ansi.green".into(), hex(palette.ansi_colors()[2]));
            style.insert("terminal.ansi.yellow".into(), hex(palette.ansi_colors()[3]));
            style.insert("terminal.ansi.blue".into(), hex(palette.ansi_colors()[4]));
            style.insert(
                "terminal.ansi.magenta".into(),
                hex(palette.ansi_colors()[5]),
            );
            style.insert("terminal.ansi.cyan".into(), hex(palette.ansi_colors()[6]));
            style.insert("terminal.ansi.white".into(), hex(palette.ansi_colors()[7]));
            style.insert(
                "terminal.ansi.bright_black".into(),
                hex(palette.ansi_colors()[8]),
            );
            style.insert(
                "terminal.ansi.bright_red".into(),
                hex(palette.ansi_colors()[9]),
            );
            style.insert(
                "terminal.ansi.bright_green".into(),
                hex(palette.ansi_colors()[10]),
            );
            style.insert(
                "terminal.ansi.bright_yellow".into(),
                hex(palette.ansi_colors()[11]),
            );
            style.insert(
                "terminal.ansi.bright_blue".into(),
                hex(palette.ansi_colors()[12]),
            );
            style.insert(
                "terminal.ansi.bright_magenta".into(),
                hex(palette.ansi_colors()[13]),
            );
            style.insert(
                "terminal.ansi.bright_cyan".into(),
                hex(palette.ansi_colors()[14]),
            );
            style.insert(
                "terminal.ansi.bright_white".into(),
                hex(palette.ansi_colors()[15]),
            );
            AppearanceColors {
                style,
                ..Default::default()
            }
        }
        Self {
            light: appearance(false, scheme),
            dark: appearance(true, scheme),
        }
    }

    /// Update untouched generated defaults while keeping custom colors.
    pub fn change_scheme(&mut self, old: PaletteScheme, new: PaletteScheme) {
        let before = Self::complete_defaults(old);
        let after = Self::complete_defaults(new);
        for (current, previous, next) in [
            (&mut self.light, before.light, after.light),
            (&mut self.dark, before.dark, after.dark),
        ] {
            for (key, value) in &mut current.style {
                if previous.style.get(key) == Some(value)
                    && let Some(replacement) = next.style.get(key)
                {
                    *value = replacement.clone();
                }
            }
        }
    }

    fn appearance(&self, dark: bool) -> &AppearanceColors {
        if dark { &self.dark } else { &self.light }
    }

    pub fn tokens(&self, mode: gpui_component::ThemeMode, scheme: PaletteScheme) -> ThemeTokens {
        let mut tokens = ThemeTokens::from_mode_with_scheme(mode, scheme);
        for (name, value) in self.appearance(mode.is_dark()).ui.iter().chain(
            self.appearance(mode.is_dark())
                .style
                .iter()
                .filter(|(key, _)| !key.starts_with("terminal.")),
        ) {
            let Some(color) = parse_hex(value) else {
                eprintln!("invalid theme color {name}: {value}");
                continue;
            };
            let normalized = name.replace('.', "_");
            match normalized.as_str() {
                "window_bg" => tokens.window_bg = rgba(color).into(),
                "text_primary" => tokens.text_primary = rgba(color).into(),
                "text_secondary" => tokens.text_secondary = rgba(color).into(),
                "text_muted" => tokens.text_muted = rgba(color).into(),
                "border" => tokens.border = rgba(color).into(),
                "border_subtle" => tokens.border_subtle = rgba(color).into(),
                "titlebar_bg" => tokens.titlebar_bg = rgba(color).into(),
                "titlebar_border" => tokens.titlebar_border = rgba(color).into(),
                "titlebar_fg" => tokens.titlebar_fg = rgba(color).into(),
                "titlebar_btn_hover" => tokens.titlebar_btn_hover = rgba(color).into(),
                "sidebar_bg" => tokens.sidebar_bg = rgba(color).into(),
                "sidebar_bg_start" => tokens.sidebar_bg_start = rgba(color).into(),
                "sidebar_bg_end" => tokens.sidebar_bg_end = rgba(color).into(),
                "sidebar_border" => tokens.sidebar_border = rgba(color).into(),
                "sidebar_fg" => tokens.sidebar_fg = rgba(color).into(),
                "sidebar_muted" => tokens.sidebar_muted = rgba(color).into(),
                "sidebar_item_hover" => tokens.sidebar_item_hover = rgba(color).into(),
                "sidebar_item_active" => tokens.sidebar_item_active = rgba(color).into(),
                "sidebar_item_active_fg" => tokens.sidebar_item_active_fg = rgba(color).into(),
                "sidebar_section_header" => tokens.sidebar_section_header = rgba(color).into(),
                "sidebar_icon_btn_hover" => tokens.sidebar_icon_btn_hover = rgba(color).into(),
                "sidebar_icon" => tokens.sidebar_icon = rgba(color).into(),
                "sidebar_icon_hover" => tokens.sidebar_icon_hover = rgba(color).into(),
                "sidebar_footer_bg" => tokens.sidebar_footer_bg = rgba(color).into(),
                "sidebar_footer_border" => tokens.sidebar_footer_border = rgba(color).into(),
                "pane_bg" => tokens.pane_bg = rgba(color).into(),
                "pane_border" => tokens.pane_border = rgba(color).into(),
                "pane_border_focused" => tokens.pane_border_focused = rgba(color).into(),
                "pane_header_bg" => tokens.pane_header_bg = rgba(color).into(),
                "pane_header_fg" => tokens.pane_header_fg = rgba(color).into(),
                "pane_header_btn_hover" => tokens.pane_header_btn_hover = rgba(color).into(),
                "pane_header_icon" => tokens.pane_header_icon = rgba(color).into(),
                "pane_header_icon_hover" => tokens.pane_header_icon_hover = rgba(color).into(),
                "primary_btn_bg" => tokens.primary_btn_bg = rgba(color).into(),
                "primary_btn_hover" => tokens.primary_btn_hover = rgba(color).into(),
                "primary_btn_fg" => tokens.primary_btn_fg = rgba(color).into(),
                "secondary_btn_bg" => tokens.secondary_btn_bg = rgba(color).into(),
                "secondary_btn_hover" => tokens.secondary_btn_hover = rgba(color).into(),
                "secondary_btn_fg" => tokens.secondary_btn_fg = rgba(color).into(),
                "card_bg" => tokens.card_bg = rgba(color).into(),
                "card_border" => tokens.card_border = rgba(color).into(),
                "card_hover" => tokens.card_hover = rgba(color).into(),
                "model_picker_bg" => tokens.model_picker_bg = rgba(color).into(),
                "model_picker_accent" => tokens.model_picker_accent = rgba(color).into(),
                "pill_active_bg" => tokens.pill_active_bg = rgba(color).into(),
                "pill_active_fg" => tokens.pill_active_fg = rgba(color).into(),
                "pill_inactive_bg" => tokens.pill_inactive_bg = rgba(color).into(),
                "pill_inactive_fg" => tokens.pill_inactive_fg = rgba(color).into(),
                "ring" => tokens.ring = rgba(color).into(),
                "block_bg" => tokens.block_bg = rgba(color).into(),
                "block_bg_hover" => tokens.block_bg_hover = rgba(color).into(),
                "block_border" => tokens.block_border = rgba(color).into(),
                "block_border_selected" => tokens.block_border_selected = rgba(color).into(),
                "block_success" => tokens.block_success = rgba(color).into(),
                "block_failure" => tokens.block_failure = rgba(color).into(),
                "block_warning" => tokens.block_warning = rgba(color).into(),
                "block_separator" => tokens.block_separator = rgba(color).into(),
                "block_status_bg" => tokens.block_status_bg = rgba(color).into(),
                "editor_bar_bg" => tokens.editor_bar_bg = rgba(color).into(),
                "editor_bar_border" => tokens.editor_bar_border = rgba(color).into(),
                "chat_user_bg" => tokens.chat_user_bg = rgba(color).into(),
                "chat_user_fg" => tokens.chat_user_fg = rgba(color).into(),
                "chat_user_border" => tokens.chat_user_border = rgba(color).into(),
                _ => eprintln!("unknown UI theme token: {name}"),
            }
        }
        tokens
    }

    pub fn palette(&self, scheme: PaletteScheme, dark: bool) -> ColorPalette {
        let mut builder = terminal::ColorPaletteBuilder::from_palette(load_palette(scheme, dark));
        for (name, value) in self
            .appearance(dark)
            .terminal
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .chain(self.appearance(dark).style.iter().filter_map(|(k, v)| {
                k.strip_prefix("terminal.")
                    .map(|name| (name.strip_prefix("ansi.").unwrap_or(name), v))
            }))
        {
            let Some(color) = parse_hex(value).filter(|c| c & 0xff == 0xff) else {
                eprintln!("invalid terminal theme color {name}: {value} (must be opaque)");
                continue;
            };
            let (r, g, b) = hex_to_rgb(color);
            builder = match name {
                "background" => builder.background(r, g, b),
                "foreground" => builder.foreground(r, g, b),
                "cursor" => builder.cursor(r, g, b),
                "black" => builder.black(r, g, b),
                "red" => builder.red(r, g, b),
                "green" => builder.green(r, g, b),
                "yellow" => builder.yellow(r, g, b),
                "blue" => builder.blue(r, g, b),
                "magenta" => builder.magenta(r, g, b),
                "cyan" => builder.cyan(r, g, b),
                "white" => builder.white(r, g, b),
                "bright_black" => builder.bright_black(r, g, b),
                "bright_red" => builder.bright_red(r, g, b),
                "bright_green" => builder.bright_green(r, g, b),
                "bright_yellow" => builder.bright_yellow(r, g, b),
                "bright_blue" => builder.bright_blue(r, g, b),
                "bright_magenta" => builder.bright_magenta(r, g, b),
                "bright_cyan" => builder.bright_cyan(r, g, b),
                "bright_white" => builder.bright_white(r, g, b),
                _ => {
                    eprintln!("unknown terminal theme token: {name}");
                    builder
                }
            };
        }
        builder.build()
    }
}

impl ThemeTokens {
    pub fn chat(&self) -> crate::agent_appearance::ChatAppearance {
        crate::agent_appearance::ChatAppearance::from_theme(self)
    }

    /// Build tokens for a mode and palette scheme. The Warp scheme reproduces
    /// Warp's default terminal chrome: near-black surfaces, white text, a cyan
    /// accent and block colors derived from Warp's ANSI palette.
    pub fn from_mode_with_scheme(mode: gpui_component::ThemeMode, scheme: PaletteScheme) -> Self {
        let mut tokens = if mode.is_dark() {
            Self::dark()
        } else {
            Self::light()
        };
        if scheme == PaletteScheme::Warp {
            tokens.apply_warp(mode.is_dark());
        }
        if scheme == PaletteScheme::MidnightGarden && mode.is_dark() {
            tokens.apply_midnight_garden();
        }
        tokens
    }

    fn apply_midnight_garden(&mut self) {
        self.window_bg = rgba(0x001536ff).into();
        self.pane_bg = rgba(0x001536ff).into();
        self.titlebar_bg = rgba(0x00112dff).into();
        self.titlebar_border = rgba(0x102a4cff).into();
        self.titlebar_fg = rgba(0xdce9ffff).into();
        self.titlebar_btn_hover = rgba(0x142d50ff).into();
        self.sidebar_bg = rgba(0x001028ff).into();
        self.sidebar_bg_start = self.sidebar_bg;
        self.sidebar_bg_end = self.sidebar_bg;
        self.sidebar_footer_bg = self.sidebar_bg;
        self.sidebar_border = rgba(0x102746ff).into();
        self.sidebar_footer_border = self.sidebar_border;
        self.sidebar_fg = rgba(0xeaf3ffff).into();
        self.sidebar_muted = rgba(0x829bbcff).into();
        self.sidebar_section_header = self.sidebar_muted;
        self.sidebar_icon = self.sidebar_muted;
        self.sidebar_item_hover = rgba(0x10284bff).into();
        self.sidebar_item_active = rgba(0x142e55ff).into();
        self.sidebar_item_active_fg = self.sidebar_fg;
        self.text_primary = rgba(0xeaf3ffff).into();
        self.text_secondary = rgba(0xa5bad7ff).into();
        self.text_muted = rgba(0x829bbcff).into();
        self.border = rgba(0x234878ff).into();
        self.border_subtle = rgba(0x102c50ff).into();
        self.pane_border = self.border_subtle;
        self.pane_header_bg = self.pane_bg;
        self.card_bg = rgba(0x061b3cff).into();
        self.card_hover = rgba(0x0a2147ff).into();
        self.card_border = rgba(0x244d80ff).into();
        self.pill_inactive_bg = rgba(0x0a2147ff).into();
        self.pill_inactive_fg = self.text_secondary;
        self.primary_btn_bg = rgba(0x1376daff).into();
        self.primary_btn_hover = rgba(0x299af0ff).into();
        self.primary_btn_fg = self.text_primary;
        self.ring = rgba(0x39a8f5ff).into();
        self.model_picker_bg = self.card_bg;
        self.model_picker_accent = self.ring;
        self.block_bg = self.pane_bg;
        self.block_bg_hover = self.card_hover;
        self.block_border = self.border_subtle;
        self.block_separator = self.border_subtle;
        self.block_border_selected = self.ring;
        self.editor_bar_bg = self.card_bg;
        self.editor_bar_border = self.card_border;
        self.chat_user_bg = self.card_hover;
        self.chat_user_fg = self.text_primary;
        self.chat_user_border = self.card_border;
    }

    /// Override surfaces, accents and block chrome with Warp's default theme.
    ///
    /// Colors come from Warp's `default_themes.rs` (background `#050505`,
    /// foreground `#FFFFFF`, accent `#19AAD8`) and its derived UI colors
    /// (`warp_core::ui::theme::color`): surfaces are the background blended
    /// with low-opacity foreground, outlines are foreground at ~4% alpha, and
    /// block selection is the accent at ~10% alpha.
    fn apply_warp(&mut self, dark: bool) {
        if dark {
            let bg = rgba(0x050505ff);
            let fg = rgba(0xffffffff);
            let accent = rgba(0x19aad8ff);
            // neutral_1 / neutral_2: background blended with foreground at
            // ~2% / ~4% alpha (Warp `internal_colors::neutral_1/2`).
            let surface_1 = rgba(0x0a0a0aff);
            let surface_2 = rgba(0x0f0f0fff);
            let surface_3 = rgba(0x141414ff);
            // outline: foreground at ~4% alpha (Warp `fg_overlay_2`).
            let outline = rgba(0xffffff1a);
            let text_sub = rgba(0xffffffb3);
            let text_disabled = rgba(0xffffff66);

            self.window_bg = bg.into();
            self.text_primary = fg.into();
            self.text_secondary = text_sub.into();
            self.text_muted = text_disabled.into();
            self.border = outline.into();
            self.border_subtle = rgba(0xffffff0d).into();

            self.titlebar_bg = bg.into();
            self.titlebar_border = rgba(0xffffff0d).into();
            self.titlebar_fg = text_sub.into();
            self.titlebar_btn_hover = surface_2.into();

            self.sidebar_bg = bg.into();
            self.sidebar_bg_start = bg.into();
            self.sidebar_bg_end = bg.into();
            self.sidebar_border = rgba(0xffffff0d).into();
            self.sidebar_fg = fg.into();
            self.sidebar_muted = text_disabled.into();
            self.sidebar_item_hover = rgba(0xffffff0d).into();
            self.sidebar_item_active = rgba(0xffffff1a).into();
            self.sidebar_item_active_fg = fg.into();
            self.sidebar_section_header = text_disabled.into();
            self.sidebar_icon_btn_hover = rgba(0xffffff1a).into();
            self.sidebar_icon = text_disabled.into();
            self.sidebar_icon_hover = fg.into();
            self.sidebar_footer_bg = bg.into();
            self.sidebar_footer_border = rgba(0xffffff0d).into();

            self.pane_bg = bg.into();
            self.pane_header_bg = bg.into();
            self.pane_header_fg = text_disabled.into();
            self.pane_header_btn_hover = surface_2.into();
            self.pane_header_icon = text_disabled.into();
            self.pane_header_icon_hover = fg.into();

            self.primary_btn_bg = accent.into();
            self.primary_btn_hover = rgba(0x3fb9e4ff).into();
            self.primary_btn_fg = rgba(0x050505ff).into();
            self.secondary_btn_bg = surface_2.into();
            self.secondary_btn_hover = surface_3.into();
            self.secondary_btn_fg = fg.into();

            self.card_bg = surface_1.into();
            self.card_border = outline.into();
            self.card_hover = surface_2.into();
            self.pill_active_bg = accent.into();
            self.pill_active_fg = rgba(0x050505ff).into();
            self.pill_inactive_bg = surface_2.into();
            self.pill_inactive_fg = text_sub.into();
            self.ring = accent.into();

            // Warp blocks sit on the terminal background, delineated by
            // dividers rather than card fills. Selection is the accent at
            // ~10% alpha; failure is Warp's ANSI red.
            self.block_bg = rgba(0x00000000).into();
            self.block_bg_hover = rgba(0xffffff08).into();
            self.block_border = outline.into();
            self.block_border_selected = accent.into();
            self.block_success = rgba(0xb4fa72ff).into();
            self.block_failure = rgba(0xff8272ff).into();
            self.block_warning = rgba(0xfefdc2ff).into();
            self.block_separator = outline.into();
            self.block_status_bg = rgba(0x00000000).into();
            self.editor_bar_bg = bg.into();
            self.editor_bar_border = outline.into();
            self.chat_user_bg = surface_3.into();
            self.chat_user_fg = fg.into();
            self.chat_user_border = outline.into();
        } else {
            let bg = rgba(0xffffffff);
            let fg = rgba(0x111111ff);
            let accent = rgba(0x00c2ffff);
            let surface_1 = rgba(0xf7f7f7ff);
            let surface_2 = rgba(0xefefefff);
            let surface_3 = rgba(0xe7e7e7ff);
            let outline = rgba(0x1111111a);
            let text_sub = rgba(0x111111b3);
            let text_disabled = rgba(0x11111166);

            self.window_bg = bg.into();
            self.text_primary = fg.into();
            self.text_secondary = text_sub.into();
            self.text_muted = text_disabled.into();
            self.border = outline.into();
            self.border_subtle = rgba(0x1111110d).into();

            self.titlebar_bg = bg.into();
            self.titlebar_border = rgba(0x1111110d).into();
            self.titlebar_fg = text_sub.into();
            self.titlebar_btn_hover = surface_2.into();

            self.sidebar_bg = bg.into();
            self.sidebar_bg_start = bg.into();
            self.sidebar_bg_end = bg.into();
            self.sidebar_border = rgba(0x1111110d).into();
            self.sidebar_fg = fg.into();
            self.sidebar_muted = text_disabled.into();
            self.sidebar_item_hover = rgba(0x1111110d).into();
            self.sidebar_item_active = rgba(0x11111108).into();
            self.sidebar_item_active_fg = fg.into();
            self.sidebar_section_header = text_disabled.into();
            self.sidebar_icon_btn_hover = rgba(0x1111111a).into();
            self.sidebar_icon = text_disabled.into();
            self.sidebar_icon_hover = fg.into();
            self.sidebar_footer_bg = bg.into();
            self.sidebar_footer_border = rgba(0x1111110d).into();

            self.pane_bg = bg.into();
            self.pane_header_bg = bg.into();
            self.pane_header_fg = text_disabled.into();
            self.pane_header_btn_hover = surface_2.into();
            self.pane_header_icon = text_disabled.into();
            self.pane_header_icon_hover = fg.into();

            self.primary_btn_bg = accent.into();
            self.primary_btn_hover = rgba(0x00a8e0ff).into();
            self.primary_btn_fg = rgba(0xffffffff).into();
            self.secondary_btn_bg = surface_2.into();
            self.secondary_btn_hover = surface_3.into();
            self.secondary_btn_fg = fg.into();

            self.card_bg = surface_1.into();
            self.card_border = outline.into();
            self.card_hover = surface_2.into();
            self.pill_active_bg = accent.into();
            self.pill_active_fg = rgba(0xffffffff).into();
            self.pill_inactive_bg = surface_2.into();
            self.pill_inactive_fg = text_sub.into();
            self.ring = accent.into();

            self.block_bg = rgba(0x00000000).into();
            self.block_bg_hover = rgba(0x11111108).into();
            self.block_border = outline.into();
            self.block_border_selected = accent.into();
            self.block_success = rgba(0x10a778ff).into();
            self.block_failure = rgba(0xc30771ff).into();
            self.block_warning = rgba(0xa89c14ff).into();
            self.block_separator = outline.into();
            self.block_status_bg = rgba(0x00000000).into();
            self.editor_bar_bg = bg.into();
            self.editor_bar_border = outline.into();
            self.chat_user_bg = surface_2.into();
            self.chat_user_fg = fg.into();
            self.chat_user_border = outline.into();
        }
    }

    pub fn dark() -> Self {
        Self {
            is_dark: true,
            window_bg: rgba(0x14151bff).into(),
            text_primary: rgba(0xe6edf3ff).into(),
            text_secondary: rgba(0x8788a5ff).into(),
            text_muted: rgba(0x625b69ff).into(),
            border: rgba(0x22232cff).into(),
            border_subtle: rgba(0x20212aff).into(),

            titlebar_bg: rgba(0x14151bff).into(),
            titlebar_border: rgba(0x20212aff).into(),
            titlebar_fg: rgba(0x8788a5ff).into(),
            titlebar_btn_hover: rgba(0x23242cff).into(),

            sidebar_bg: rgba(0x121319ff).into(),
            sidebar_bg_start: rgba(0x121319ff).into(),
            sidebar_bg_end: rgba(0x121319ff).into(),
            sidebar_border: rgba(0x242531ff).into(),
            sidebar_fg: rgba(0xe6edf3ff).into(),
            sidebar_muted: rgba(0x8b879eff).into(),
            sidebar_item_hover: rgba(0xffffff15).into(),
            sidebar_item_active: rgba(0xffffff16).into(),
            sidebar_item_active_fg: rgba(0xffffffff).into(),
            sidebar_section_header: rgba(0x9a95adff).into(),
            sidebar_icon_btn_hover: rgba(0xffffff1a).into(),
            sidebar_icon: rgba(0x9a95adff).into(),
            sidebar_icon_hover: rgba(0xffffffff).into(),
            sidebar_footer_bg: rgba(0x13141aff).into(),
            sidebar_footer_border: rgba(0x242531ff).into(),

            pane_bg: rgba(0x0c0d12ff).into(),
            pane_border: rgba(0x3c4149ff).into(),
            pane_border_focused: rgba(0x8799adff).into(),
            pane_header_bg: rgba(0x0c0d12ff).into(),
            pane_header_fg: rgba(0x918995ff).into(),
            pane_header_btn_hover: rgba(0x23242cff).into(),
            pane_header_icon: rgba(0x817987ff).into(),
            pane_header_icon_hover: rgba(0xf0f6fcff).into(),

            primary_btn_bg: rgba(0x1f6febff).into(),
            primary_btn_hover: rgba(0x2f81f7ff).into(),
            primary_btn_fg: rgba(0xffffffff).into(),
            secondary_btn_bg: rgba(0x26272fff).into(),
            secondary_btn_hover: rgba(0x2d2e38ff).into(),
            secondary_btn_fg: rgba(0xffffffff).into(),

            card_bg: rgba(0x1a1b24ff).into(),
            card_border: rgba(0x2b2c3cff).into(),
            card_hover: rgba(0x222330ff).into(),
            model_picker_bg: rgba(0x181818ff).into(),
            model_picker_accent: rgba(0x3b6ef5ff).into(),
            pill_active_bg: rgba(0x1f6febff).into(),
            pill_active_fg: rgba(0xffffffff).into(),
            pill_inactive_bg: rgba(0x23242cff).into(),
            pill_inactive_fg: rgba(0x918995ff).into(),
            ring: rgba(0x388bfdff).into(),

            block_bg: rgba(0x11151aff).into(),
            block_bg_hover: rgba(0x151a21ff).into(),
            block_border: rgba(0x1f242bff).into(),
            block_border_selected: rgba(0x4c9aedff).into(),
            block_success: rgba(0x3fb950ff).into(),
            block_failure: rgba(0xf85149ff).into(),
            block_warning: rgba(0xd29922ff).into(),
            block_separator: rgba(0x1b2027ff).into(),
            block_status_bg: rgba(0x0d1116ff).into(),
            editor_bar_bg: rgba(0x0d1014ff).into(),
            editor_bar_border: rgba(0x1f242bff).into(),
            chat_user_bg: rgba(0x1a1b24ff).into(),
            chat_user_fg: rgba(0xe6edf3ff).into(),
            chat_user_border: rgba(0x2b2c3cff).into(),
        }
    }

    pub fn light() -> Self {
        Self {
            is_dark: false,
            window_bg: rgba(0xf6f8faff).into(),
            text_primary: rgba(0x1f2328ff).into(),
            text_secondary: rgba(0x59636eff).into(),
            text_muted: rgba(0x746d7aff).into(),
            border: rgba(0xd1d9e0ff).into(),
            border_subtle: rgba(0xe1e4e8ff).into(),

            titlebar_bg: rgba(0xffffffff).into(),
            titlebar_border: rgba(0xd1d9e0ff).into(),
            titlebar_fg: rgba(0x1f2328ff).into(),
            titlebar_btn_hover: rgba(0xeaeef2ff).into(),

            sidebar_bg: rgba(0xffffffff).into(),
            sidebar_bg_start: rgba(0xffffffff).into(),
            sidebar_bg_end: rgba(0xffffffff).into(),
            sidebar_border: rgba(0xd1d9e0ff).into(),
            sidebar_fg: rgba(0x1f2328ff).into(),
            sidebar_muted: rgba(0x656d76ff).into(),
            sidebar_item_hover: rgba(0x0000000a).into(),
            sidebar_item_active: rgba(0x00000008).into(),
            sidebar_item_active_fg: rgba(0x1f2328ff).into(),
            sidebar_section_header: rgba(0x8c9095ff).into(),
            sidebar_icon_btn_hover: rgba(0x0000000e).into(),
            sidebar_icon: rgba(0x656d76ff).into(),
            sidebar_icon_hover: rgba(0x1f2328ff).into(),
            sidebar_footer_bg: rgba(0xffffffff).into(),
            sidebar_footer_border: rgba(0xd1d9e0ff).into(),

            pane_bg: rgba(0xffffffff).into(),
            pane_border: rgba(0xc0c6ceff).into(),
            pane_border_focused: rgba(0x71849aff).into(),
            pane_header_bg: rgba(0xf6f8faff).into(),
            pane_header_fg: rgba(0x59636eff).into(),
            pane_header_btn_hover: rgba(0xeaeef2ff).into(),
            pane_header_icon: rgba(0x59636eff).into(),
            pane_header_icon_hover: rgba(0x1f2328ff).into(),

            primary_btn_bg: rgba(0x0969daff).into(),
            primary_btn_hover: rgba(0x0854b0ff).into(),
            primary_btn_fg: rgba(0xffffffff).into(),
            secondary_btn_bg: rgba(0xeaeef2ff).into(),
            secondary_btn_hover: rgba(0xdfe4e8ff).into(),
            secondary_btn_fg: rgba(0x1f2328ff).into(),

            card_bg: rgba(0xffffffff).into(),
            card_border: rgba(0xd1d9e0ff).into(),
            card_hover: rgba(0xf6f8faff).into(),
            model_picker_bg: rgba(0xffffffff).into(),
            model_picker_accent: rgba(0x2857d5ff).into(),
            pill_active_bg: rgba(0x0969daff).into(),
            pill_active_fg: rgba(0xffffffff).into(),
            pill_inactive_bg: rgba(0xeaeef2ff).into(),
            pill_inactive_fg: rgba(0x59636eff).into(),
            ring: rgba(0x0969daff).into(),

            block_bg: rgba(0xffffffff).into(),
            block_bg_hover: rgba(0xf6f8faff).into(),
            block_border: rgba(0xd1d9e0ff).into(),
            block_border_selected: rgba(0x0969daff).into(),
            block_success: rgba(0x1a7f37ff).into(),
            block_failure: rgba(0xcf222eff).into(),
            block_warning: rgba(0x9a6700ff).into(),
            block_separator: rgba(0xeaeef2ff).into(),
            block_status_bg: rgba(0xf6f8faff).into(),
            editor_bar_bg: rgba(0xffffffff).into(),
            editor_bar_border: rgba(0xd1d9e0ff).into(),
            chat_user_bg: rgba(0xf6f8faff).into(),
            chat_user_fg: rgba(0x1f2328ff).into(),
            chat_user_border: rgba(0xd1d9e0ff).into(),
        }
    }
}

mod palette;
pub use palette::{hex_to_rgb, load_palette, parse_hex};
#[cfg(test)]
use palette::{load_github_palette, load_warp_palette};

#[cfg(test)]
mod tests;
