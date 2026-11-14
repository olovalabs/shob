//! Terminal color parsing and manipulation.
//!
//! Provides utilities for parsing terminal color schemes and OSC 11 responses.

/// RGB color representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    /// Create a new RGB color.
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Create from a hex string (e.g., "#ff0000" or "ff0000").
    pub fn from_hex(hex: &str) -> Option<Self> {
        let hex = hex.trim_start_matches('#');
        if hex.len() != 6 {
            return None;
        }

        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;

        Some(Self::new(r, g, b))
    }

    /// Convert to hex string.
    pub fn to_hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Calculate relative luminance (0.0 - 1.0).
    pub fn luminance(&self) -> f64 {
        let r = self.r as f64 / 255.0;
        let g = self.g as f64 / 255.0;
        let b = self.b as f64 / 255.0;

        // sRGB luminance formula
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// Check if this is a "dark" color.
    pub fn is_dark(&self) -> bool {
        self.luminance() < 0.5
    }

    /// Check if this is a "light" color.
    pub fn is_light(&self) -> bool {
        !self.is_dark()
    }

    /// Convert to ANSI escape sequence for foreground.
    pub fn to_fg_escape(&self) -> String {
        format!("\x1b[38;2;{};{};{}m", self.r, self.g, self.b)
    }

    /// Convert to ANSI escape sequence for background.
    pub fn to_bg_escape(&self) -> String {
        format!("\x1b[48;2;{};{};{}m", self.r, self.g, self.b)
    }
}

impl Default for RgbColor {
    fn default() -> Self {
        Self::new(0, 0, 0)
    }
}

/// Terminal color scheme.
#[derive(Debug, Clone)]
pub struct TerminalColorScheme {
    /// Foreground color.
    pub foreground: RgbColor,
    /// Background color.
    pub background: RgbColor,
    /// Cursor color.
    pub cursor: Option<RgbColor>,
    /// Selection background.
    pub selection_bg: Option<RgbColor>,
    /// Selection foreground.
    pub selection_fg: Option<RgbColor>,
    /// ANSI color palette (16 colors).
    pub palette: Option<[RgbColor; 16]>,
}

impl Default for TerminalColorScheme {
    fn default() -> Self {
        Self {
            foreground: RgbColor::new(204, 204, 204),
            background: RgbColor::new(0, 0, 0),
            cursor: None,
            selection_bg: None,
            selection_fg: None,
            palette: None,
        }
    }
}

impl TerminalColorScheme {
    /// Check if the terminal has a dark background.
    pub fn is_dark(&self) -> bool {
        self.background.is_dark()
    }

    /// Check if the terminal has a light background.
    pub fn is_light(&self) -> bool {
        self.background.is_light()
    }
}

/// Parse OSC 11 background color response.
/// Format: \x1b]11;rgb:RRRR/GGGG/BBBB\x07 or \x1b]11;rgb:RR/GG/BB\x07
pub fn parse_osc11_background_color(response: &str) -> Option<RgbColor> {
    // Strip OSC prefix and suffix
    let response = response.trim();

    // Check for OSC 11 response format
    if !response.starts_with("\x1b]11;") {
        return None;
    }

    // Strip OSC prefix
    let content = &response[5..];

    // Strip terminator (BEL or ST)
    let content = if content.ends_with('\x07') {
        &content[..content.len() - 1]
    } else if content.ends_with("\x1b\\") {
        &content[..content.len() - 2]
    } else {
        content
    };

    // Parse color
    parse_color_response(content)
}

/// Parse a color response string.
/// Supports formats:
/// - rgb:RRRR/GGGG/BBBB (16-bit per channel)
/// - rgb:RR/GG/BB (8-bit per channel)
/// - #RRGGBB
/// - rgb:RR,GG,BB
fn parse_color_response(content: &str) -> Option<RgbColor> {
    // Try rgb: format
    if let Some(rest) = content.strip_prefix("rgb:") {
        // Try RRRR/GGGG/BBBB or RR/GG/BB format
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 3 {
            let r = parse_color_component(parts[0])?;
            let g = parse_color_component(parts[1])?;
            let b = parse_color_component(parts[2])?;
            return Some(RgbColor::new(r, g, b));
        }

        // Try RR,GG,BB format
        let parts: Vec<&str> = rest.split(',').collect();
        if parts.len() == 3 {
            let r = parse_color_component(parts[0])?;
            let g = parse_color_component(parts[1])?;
            let b = parse_color_component(parts[2])?;
            return Some(RgbColor::new(r, g, b));
        }
    }

    // Try hex format
    if content.starts_with('#') {
        return RgbColor::from_hex(content);
    }

    None
}

/// Parse a single color component.
/// Supports both 8-bit (RR) and 16-bit (RRRR) hex formats.
fn parse_color_component(s: &str) -> Option<u8> {
    let s = s.trim();

    match s.len() {
        2 => u8::from_str_radix(s, 16).ok(),
        4 => {
            // 16-bit: take the high byte
            let high = &s[0..2];
            u8::from_str_radix(high, 16).ok()
        }
        _ => None,
    }
}

/// Parse terminal color scheme report.
/// This is a more comprehensive parsing that handles multiple color queries.
pub fn parse_terminal_color_scheme_report(response: &str) -> Option<TerminalColorScheme> {
    let mut scheme = TerminalColorScheme::default();

    // Split by OSC sequences
    let mut pos = 0;
    let bytes = response.as_bytes();

    while pos < bytes.len() {
        if bytes[pos] == 0x1b && pos + 1 < bytes.len() && bytes[pos + 1] == b']' {
            // Find end of OSC sequence
            let start = pos;
            pos += 2;

            // Find terminator
            while pos < bytes.len() {
                if bytes[pos] == 0x07 {
                    pos += 1;
                    break;
                } else if bytes[pos] == 0x1b && pos + 1 < bytes.len() && bytes[pos + 1] == b'\\' {
                    pos += 2;
                    break;
                }
                pos += 1;
            }

            // Parse the OSC sequence
            let osc = &response[start..pos];
            if let Some(color) = parse_osc_color(osc) {
                if osc.starts_with("\x1b]11;") {
                    scheme.background = color;
                } else if osc.starts_with("\x1b]10;") {
                    scheme.foreground = color;
                } else if osc.starts_with("\x1b]12;") {
                    scheme.cursor = Some(color);
                }
            }
        } else {
            pos += 1;
        }
    }

    Some(scheme)
}

/// Parse a single OSC color sequence.
fn parse_osc_color(osc: &str) -> Option<RgbColor> {
    if osc.starts_with("\x1b]11;") {
        parse_osc11_background_color(osc)
    } else if osc.starts_with("\x1b]10;") {
        // Foreground color
        let content = &osc[5..];
        let content = if content.ends_with('\x07') {
            &content[..content.len() - 1]
        } else if content.ends_with("\x1b\\") {
            &content[..content.len() - 2]
        } else {
            content
        };
        parse_color_response(content)
    } else if osc.starts_with("\x1b]12;") {
        // Cursor color
        let content = &osc[5..];
        let content = if content.ends_with('\x07') {
            &content[..content.len() - 1]
        } else if content.ends_with("\x1b\\") {
            &content[..content.len() - 2]
        } else {
            content
        };
        parse_color_response(content)
    } else {
        None
    }
}

/// Query terminal for background color.
/// Returns the query string to send to the terminal.
pub fn query_background_color() -> &'static str {
    "\x1b]11;?\x07"
}

/// Query terminal for foreground color.
pub fn query_foreground_color() -> &'static str {
    "\x1b]10;?\x07"
}

/// Query terminal for cursor color.
pub fn query_cursor_color() -> &'static str {
    "\x1b]12;?\x07"
}

/// Query terminal for all color scheme information.
pub fn query_color_scheme() -> String {
    format!(
        "{}{}{}",
        query_background_color(),
        query_foreground_color(),
        query_cursor_color()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgb_color() {
        let color = RgbColor::new(255, 128, 0);
        assert_eq!(color.to_hex(), "#ff8000");
        assert!(color.is_light());
    }

    #[test]
    fn test_rgb_from_hex() {
        let color = RgbColor::from_hex("#ff0000").unwrap();
        assert_eq!(color.r, 255);
        assert_eq!(color.g, 0);
        assert_eq!(color.b, 0);
    }

    #[test]
    fn test_parse_osc11_background() {
        let response = "\x1b]11;rgb:1e1e/1e1e/1e1e\x07";
        let color = parse_osc11_background_color(response).unwrap();
        assert_eq!(color.r, 0x1e);
    }

    #[test]
    fn test_luminance() {
        let black = RgbColor::new(0, 0, 0);
        let white = RgbColor::new(255, 255, 255);

        assert!(black.is_dark());
        assert!(white.is_light());
    }
}
