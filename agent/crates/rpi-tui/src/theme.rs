//! Theme system for TUI customization.
//!
//! Provides color schemes and styling options.

use std::sync::Mutex;

/// Theme colors.
///
/// Mirrors the pi `ThemeJson.colors` tokens (see
/// `.reference/pi/.../theme/dark.json`). The semantic core (`muted`, `accent`,
/// `error`, `success`, `warning`, `border`, `surface`) drives most components;
/// the `md_*` / `tool_*` / `thinking_*` fields correspond 1:1 to the pi md* /
/// toolDiff* / thinking-level tokens and let the markdown renderer and tool
/// blocks match pi's palette instead of hardcoded `Ansi256`/`fg_256` literals.
#[derive(Debug, Clone)]
pub struct ThemeColors {
    /// Primary text color.
    pub text: Color,
    /// Muted/dimmed text color (pi `muted`).
    pub muted: Color,
    /// Dimmer-than-muted color (pi `dim`).
    pub dim: Color,
    /// Accent/highlight color (pi `accent`, e.g. teal-ish).
    pub accent: Color,
    /// Error color.
    pub error: Color,
    /// Success color.
    pub success: Color,
    /// Warning color.
    pub warning: Color,
    /// Info color.
    pub info: Color,
    /// Background color.
    pub background: Color,
    /// Primary surface color (pi `userMessageBg`).
    pub surface: Color,
    /// Border color (pi `border`).
    pub border: Color,
    /// Accent border color (pi `borderAccent`).
    pub border_accent: Color,
    /// Muted border color (pi `borderMuted`).
    pub border_muted: Color,
    /// Selection background color.
    pub selection: Color,
    /// Cursor color.
    pub cursor: Color,
    /// Text color for thinking/reasoning blocks (pi `thinkingText`).
    pub thinking_text: Color,

    // -- Markdown syntax tokens (pi md*) --
    /// Heading color (pi `mdHeading`, gold on dark).
    pub md_heading: Color,
    /// Inline link color (pi `mdLink`).
    pub md_link: Color,
    /// Link URL color (pi `mdLinkUrl`).
    pub md_link_url: Color,
    /// Inline `code` color (pi `mdCode`).
    pub md_code: Color,
    /// Subtle inline-code background. This Rust extension keeps code readable
    /// without applying a card background to the whole assistant message.
    pub md_code_bg: Color,
    /// Code-block body color (pi `mdCodeBlock`).
    pub md_code_block: Color,
    /// Low-contrast code-block surface (One Dark inspired).
    pub md_code_block_bg: Color,
    /// Code-block fence/border color (pi `mdCodeBlockBorder`).
    pub md_code_block_border: Color,
    /// Blockquote body color (pi `mdQuote`).
    pub md_quote: Color,
    /// Blockquote border color (pi `mdQuoteBorder`).
    pub md_quote_border: Color,
    /// Horizontal rule color (pi `mdHr`).
    pub md_hr: Color,
    /// List bullet color (pi `mdListBullet`).
    pub md_list_bullet: Color,

    // -- Tool execution backgrounds (pi tool*Bg / toolTitle / toolOutput) --
    /// Pending-tool background.
    pub tool_pending_bg: Color,
    /// Successful-tool background.
    pub tool_success_bg: Color,
    /// Failed-tool background.
    pub tool_error_bg: Color,
    /// Tool title text color.
    pub tool_title: Color,
    /// Tool output text color.
    pub tool_output: Color,
    /// Bash mode accent color (pi `bashMode`).
    pub bash_mode: Color,

    // -- Custom messages / skill invocation (pi customMessage*) --
    /// Skill-invocation / custom-message background (pi `customMessageBg`).
    pub custom_message_bg: Color,
    /// Skill-invocation `[skill]` label color (pi `customMessageLabel`).
    pub custom_message_label: Color,
    /// Skill-invocation body text color (pi `customMessageText`).
    pub custom_message_text: Color,

    // -- Tool diffs (pi toolDiff*) --
    /// Added-line color.
    pub tool_diff_added: Color,
    /// Removed-line color.
    pub tool_diff_removed: Color,
    /// Context-line color.
    pub tool_diff_context: Color,
}

impl Default for ThemeColors {
    fn default() -> Self {
        // Defaults mirror the pi `dark.json` palette (`accent` teal, the
        // `#f0c674` gold heading, `#b5bd68` green code blocks, and the
        // `#282832`/`#283228`/`#3c2828` tool backgrounds). Exact pi hexes are
        // approximated by their nearest ANSI-256 index so 8/256-color
        // terminals reproduce the same relationships; truecolor rendering is
        // handled per-`Color` variant at emit time.
        Self {
            text: Color::Default,
            muted: Color::Ansi256(244),   // gray (#808080-ish)
            dim: Color::Ansi256(242),     // dimGray (#666666-ish)
            accent: Color::Ansi256(108),  // accent teal (#8abeb7) ≈ 108
            error: Color::Ansi256(131),   // red (#cc6666) ≈ 131
            success: Color::Ansi256(107), // green (#b5bd68) ≈ 107
            warning: Color::Ansi256(226), // yellow
            info: Color::Ansi256(81),     // cyan
            background: Color::Default,
            surface: Color::Ansi256(236), // userMessageBg (#343541) ≈ 236
            border: Color::Ansi256(67),   // border blue (#5f87ff) ≈ 67
            border_accent: Color::Ansi256(45), // borderAccent cyan (#00d7ff) ≈ 45
            border_muted: Color::Ansi256(239), // borderMuted darkGray (#505050) ≈ 239
            selection: Color::Ansi256(60), // selectedBg (#3a3a4a) ≈ 60
            cursor: Color::Ansi256(81),
            thinking_text: Color::Ansi256(244), // thinkingText gray

            // Markdown
            md_heading: Color::Ansi256(179),  // gold (#f0c674) ≈ 179
            md_link: Color::Ansi256(110),     // blue (#81a2be) ≈ 110
            md_link_url: Color::Ansi256(242), // dimGray
            md_code: Color::Rgb(138, 190, 183), // upstream accent #8abeb7
            md_code_bg: Color::Rgb(48, 52, 59), // subtle inline chip
            md_code_block: Color::Rgb(212, 212, 212), // neutral code text
            md_code_block_bg: Color::Rgb(40, 44, 52), // One Dark #282c34
            md_code_block_border: Color::Ansi256(244), // gray
            md_quote: Color::Ansi256(244),    // gray
            md_quote_border: Color::Ansi256(244),
            md_hr: Color::Ansi256(244),
            md_list_bullet: Color::Ansi256(108), // accent

            // Tool blocks
            // Keep status backgrounds close to the terminal surface. Status is
            // communicated by the icon/foreground; a full saturated green
            // panel makes successful tools dominate the transcript.
            tool_pending_bg: Color::Rgb(40, 44, 52),
            tool_success_bg: Color::Rgb(38, 48, 52), // subtle blue/teal tint
            tool_error_bg: Color::Rgb(55, 40, 45),   // subtle red tint
            tool_title: Color::Default,
            tool_output: Color::Ansi256(244),
            bash_mode: Color::Ansi256(107), // green

            // Custom messages / skill invocation (pi dark.json):
            // customMsgBg #2d2838, customMessageLabel #9575cd, text #d4d4d4.
            custom_message_bg: Color::Rgb(45, 40, 56),
            custom_message_label: Color::Rgb(149, 117, 205),
            custom_message_text: Color::Default,

            // Tool diffs
            tool_diff_added: Color::Ansi256(107),   // green
            tool_diff_removed: Color::Ansi256(131), // red
            tool_diff_context: Color::Ansi256(244), // gray
        }
    }
}

/// Color representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Default terminal color.
    Default,
    /// ANSI 16-color palette.
    Ansi16(u8),
    /// ANSI 256-color palette.
    Ansi256(u8),
    /// RGB true color.
    Rgb(u8, u8, u8),
}

impl Color {
    /// Convert to ANSI foreground escape sequence.
    pub fn to_fg(&self) -> String {
        match self {
            Color::Default => "\x1b[39m".to_string(),
            Color::Ansi16(c) => format!("\x1b[{}m", c),
            Color::Ansi256(c) => format!("\x1b[38;5;{}m", c),
            Color::Rgb(r, g, b) => format!("\x1b[38;2;{};{};{}m", r, g, b),
        }
    }

    /// Convert to ANSI background escape sequence.
    pub fn to_bg(&self) -> String {
        match self {
            Color::Default => "\x1b[49m".to_string(),
            Color::Ansi16(c) => format!("\x1b[{}m", c + 10),
            Color::Ansi256(c) => format!("\x1b[48;5;{}m", c),
            Color::Rgb(r, g, b) => format!("\x1b[48;2;{};{};{}m", r, g, b),
        }
    }

    /// Apply color to text (foreground).
    pub fn fg(&self, text: &str) -> String {
        format!("{}{}\x1b[0m", self.to_fg(), text)
    }

    /// Apply color to text (background).
    pub fn bg(&self, text: &str) -> String {
        let bg = self.to_bg();
        // Foreground helpers deliberately terminate with SGR 0. When such a
        // styled span is nested in a panel background, that reset also clears
        // the background and used to leave the rest of tool/user rows patchy.
        // Re-apply this background after every nested reset.
        let nested = text.replace("\x1b[0m", &format!("\x1b[0m{bg}"));
        format!("{bg}{nested}\x1b[0m")
    }
}

/// Spacing tokens for terminal components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeSpacing {
    /// Horizontal inset on each side of a tool panel, in terminal columns.
    pub tool_panel_padding_x: usize,
    /// Vertical inset above and below a tool panel, in terminal rows.
    pub tool_panel_padding_y: usize,
}

impl Default for ThemeSpacing {
    fn default() -> Self {
        Self {
            tool_panel_padding_x: 2,
            tool_panel_padding_y: 1,
        }
    }
}

/// Theme settings.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Color scheme.
    pub colors: ThemeColors,
    /// Component spacing shared by bash and generic tool execution panels.
    pub spacing: ThemeSpacing,
    /// Border style.
    pub border_style: BorderStyle,
    /// Corner style.
    pub corner_style: CornerStyle,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            colors: ThemeColors::default(),
            spacing: ThemeSpacing::default(),
            border_style: BorderStyle::Rounded,
            corner_style: CornerStyle::Rounded,
        }
    }
}

/// Border style for boxes.
#[derive(Debug, Clone, Copy, Default)]
pub enum BorderStyle {
    #[default]
    Rounded,
    Sharp,
    Double,
    Thick,
    None,
}

impl BorderStyle {
    /// Get the border characters.
    pub fn chars(&self) -> BorderChars {
        match self {
            BorderStyle::Rounded => BorderChars {
                top_left: "╭",
                top_right: "╮",
                bottom_left: "╰",
                bottom_right: "╯",
                horizontal: "─",
                vertical: "│",
            },
            BorderStyle::Sharp => BorderChars {
                top_left: "┌",
                top_right: "┐",
                bottom_left: "└",
                bottom_right: "┘",
                horizontal: "─",
                vertical: "│",
            },
            BorderStyle::Double => BorderChars {
                top_left: "╔",
                top_right: "╗",
                bottom_left: "╚",
                bottom_right: "╝",
                horizontal: "═",
                vertical: "║",
            },
            BorderStyle::Thick => BorderChars {
                top_left: "┏",
                top_right: "┓",
                bottom_left: "┗",
                bottom_right: "┛",
                horizontal: "━",
                vertical: "┃",
            },
            BorderStyle::None => BorderChars {
                top_left: " ",
                top_right: " ",
                bottom_left: " ",
                bottom_right: " ",
                horizontal: " ",
                vertical: " ",
            },
        }
    }
}

/// Border characters.
pub struct BorderChars {
    pub top_left: &'static str,
    pub top_right: &'static str,
    pub bottom_left: &'static str,
    pub bottom_right: &'static str,
    pub horizontal: &'static str,
    pub vertical: &'static str,
}

/// Corner style.
#[derive(Debug, Clone, Copy, Default)]
pub enum CornerStyle {
    #[default]
    Rounded,
    Sharp,
}

/// A theme palette and its cache revision.
pub struct ThemeManager {
    current: Mutex<Theme>,
    revision: std::sync::atomic::AtomicU64,
}

/// Current process-wide theme revision, bumped on every global theme mutation.
///
/// Components that memoize rendered output (which bakes in theme colors) key
/// their cache on this value, so a live theme switch invalidates them without
/// every host having to walk the tree and call `invalidate` (upstream's
/// `theme-controller.ts` does both: swap the palette and `ui.invalidate()`).
pub fn theme_revision() -> u64 {
    global_theme_manager()
        .revision
        .load(std::sync::atomic::Ordering::Acquire)
}

impl ThemeManager {
    /// Create a new theme manager.
    pub fn new() -> Self {
        Self {
            current: Mutex::new(Theme::default()),
            revision: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Get the current theme.
    pub fn get(&self) -> Theme {
        self.current.lock().map(|t| t.clone()).unwrap_or_default()
    }

    /// Set the theme.
    pub fn set(&self, theme: Theme) {
        if let Ok(mut current) = self.current.lock() {
            *current = theme;
            self.revision
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }

    /// Apply a preset theme to this manager.
    pub fn apply_preset(&self, preset: ThemePreset) {
        self.set(theme_for_preset(preset));
    }
}

/// Build a complete theme for a preset.
///
/// Kept separate from [`ThemeManager::apply_preset`] so the process-wide theme
/// used by components can be updated without maintaining a second palette.
fn theme_for_preset(preset: ThemePreset) -> Theme {
    match preset {
        ThemePreset::Dark => Theme::default(),
        ThemePreset::Light => {
            let mut colors = ThemeColors::default();
            colors.text = Color::Default;
            colors.muted = Color::Ansi256(241); // mediumGray (#6c6c6c)
            colors.dim = Color::Ansi256(243); // dimGray (#767676)
            colors.accent = Color::Ansi256(66); // teal (#5a8080)
            colors.error = Color::Ansi256(131); // red (#aa5555)
            colors.success = Color::Ansi256(65); // green (#588458)
            colors.warning = Color::Ansi256(136); // yellow (#9a7326)
            colors.info = Color::Ansi256(67); // blue
            colors.background = Color::Default;
            colors.surface = Color::Ansi256(254); // userMsgBg (#e8e8e8)
            colors.border = Color::Ansi256(67); // blue (#547da7)
            colors.border_accent = Color::Ansi256(66); // teal
            colors.border_muted = Color::Ansi256(249); // lightGray (#b0b0b0)
            colors.selection = Color::Ansi256(189); // selectedBg (#d0d0e0)
            colors.cursor = Color::Ansi256(67);
            colors.thinking_text = Color::Ansi256(241);
            colors.md_heading = Color::Ansi256(136); // yellow
            colors.md_link = Color::Ansi256(67); // blue
            colors.md_link_url = Color::Ansi256(243);
            colors.md_code = Color::Rgb(90, 128, 128); // native light teal
            colors.md_code_bg = Color::Rgb(232, 234, 237);
            colors.md_code_block = Color::Rgb(31, 35, 40);
            colors.md_code_block_bg = Color::Rgb(246, 248, 250); // GitHub light code
            colors.md_code_block_border = Color::Ansi256(241);
            colors.md_quote = Color::Ansi256(241);
            colors.md_quote_border = Color::Ansi256(241);
            colors.md_hr = Color::Ansi256(241);
            colors.md_list_bullet = Color::Ansi256(65); // green
            colors.tool_pending_bg = Color::Ansi256(189);
            colors.tool_success_bg = Color::Ansi256(151);
            colors.tool_error_bg = Color::Ansi256(181);
            colors.tool_title = Color::Default;
            colors.tool_output = Color::Ansi256(241);
            colors.bash_mode = Color::Ansi256(65);
            // Custom messages / skill invocation (pi light.json):
            // customMsgBg #ede7f6, customMessageLabel #7e57c2, text #1f2328.
            colors.custom_message_bg = Color::Rgb(237, 231, 246);
            colors.custom_message_label = Color::Rgb(126, 87, 194);
            colors.custom_message_text = Color::Ansi256(237);
            colors.tool_diff_added = Color::Ansi256(65);
            colors.tool_diff_removed = Color::Ansi256(131);
            colors.tool_diff_context = Color::Ansi256(241);
            Theme {
                colors,
                ..Default::default()
            }
        }
        ThemePreset::Monochrome => {
            let mut colors = ThemeColors::default();
            for f in [
                &mut colors.accent,
                &mut colors.border_accent,
                &mut colors.md_code,
                &mut colors.md_list_bullet,
                &mut colors.bash_mode,
            ] {
                *f = Color::Ansi256(15);
            }
            colors.muted = Color::Ansi256(244);
            colors.dim = Color::Ansi256(242);
            colors.error = Color::Ansi256(9);
            colors.success = Color::Ansi256(15);
            colors.warning = Color::Ansi256(15);
            colors.info = Color::Ansi256(15);
            colors.surface = Color::Ansi256(236);
            colors.border = Color::Ansi256(244);
            colors.border_muted = Color::Ansi256(240);
            colors.selection = Color::Ansi256(244);
            colors.cursor = Color::Ansi256(15);
            colors.thinking_text = Color::Ansi256(244);
            colors.md_heading = Color::Ansi256(15);
            colors.md_link = Color::Ansi256(15);
            colors.md_link_url = Color::Ansi256(242);
            colors.md_code_bg = Color::Ansi256(237);
            colors.md_code_block = Color::Ansi256(15);
            colors.md_code_block_bg = Color::Ansi256(235);
            colors.md_code_block_border = Color::Ansi256(244);
            colors.md_quote = Color::Ansi256(244);
            colors.md_quote_border = Color::Ansi256(244);
            colors.md_hr = Color::Ansi256(244);
            colors.custom_message_bg = Color::Ansi256(236);
            colors.custom_message_label = Color::Ansi256(15);
            colors.custom_message_text = Color::Ansi256(244);
            colors.tool_title = Color::Default;
            colors.tool_output = Color::Ansi256(244);
            colors.tool_diff_added = Color::Ansi256(15);
            colors.tool_diff_removed = Color::Ansi256(9);
            colors.tool_diff_context = Color::Ansi256(244);
            Theme {
                colors,
                ..Default::default()
            }
        }
    }
}

impl Default for ThemeManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Theme presets.
#[derive(Debug, Clone, Copy)]
pub enum ThemePreset {
    Dark,
    Light,
    Monochrome,
}

/// Global theme helper functions.
pub fn theme() -> Theme {
    global_theme_manager().get()
}

/// Apply a preset to the process-wide theme read by every component.
///
/// Creating a standalone [`ThemeManager`] does not affect components that call
/// [`theme`]. Hosts should use this function for live theme switching.
pub fn apply_theme_preset(preset: ThemePreset) {
    global_theme_manager().apply_preset(preset);
}

/// Return the process-wide theme manager.
pub fn global_theme_manager() -> &'static ThemeManager {
    THEME_MANAGER.get_or_init(ThemeManager::new)
}

/// Global theme manager.
static THEME_MANAGER: std::sync::OnceLock<ThemeManager> = std::sync::OnceLock::new();

/// Apply color from theme.
pub fn themed_text(text: &str, color_fn: fn(&ThemeColors) -> Color) -> String {
    let theme = theme();
    let color = color_fn(&theme.colors);
    color.fg(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_color_ansi() {
        let color = Color::Ansi256(196);
        let text = color.fg("Hello");
        assert!(text.contains("Hello"));
    }

    #[test]
    fn test_theme_manager() {
        let global_revision = theme_revision();
        let manager = ThemeManager::new();
        manager.apply_preset(ThemePreset::Dark);
        let theme = manager.get();
        assert!(matches!(theme.colors.accent, Color::Ansi256(108)));
        assert_eq!(
            manager.revision.load(std::sync::atomic::Ordering::Acquire),
            1
        );
        assert_eq!(theme_revision(), global_revision);
    }
}
