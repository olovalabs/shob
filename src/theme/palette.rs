use super::*;

pub fn parse_hex(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#')?;
    if hex.len() == 6 {
        u32::from_str_radix(&format!("{hex}ff"), 16).ok()
    } else if hex.len() == 8 {
        u32::from_str_radix(hex, 16).ok()
    } else {
        None
    }
}

pub fn hex_to_rgb(hex: u32) -> (u8, u8, u8) {
    (
        ((hex >> 24) & 0xff) as u8,
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
    )
}

pub fn load_github_palette(dark: bool) -> ColorPalette {
    let theme_data = crate::assets::AppAssets::get("themes/github.json");
    let target_name = if dark { "GitHub Dark" } else { "GitHub Light" };
    if let Some(file) = theme_data
        && let Ok(Value::Array(themes)) = serde_json::from_slice::<Value>(&file.data)
        && let Some(target_theme) = themes
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some(target_name))
    {
        let mut builder = ColorPalette::builder();
        let style = target_theme.get("style").and_then(Value::as_object);

        let get_color = |key: &str| -> Option<(u8, u8, u8)> {
            style
                .and_then(|s| s.get(key))
                .and_then(Value::as_str)
                .and_then(parse_hex)
                .map(hex_to_rgb)
        };

        if let Some((r, g, b)) = get_color("terminal.background")
            .or_else(|| get_color("editor.background"))
            .or_else(|| get_color("background"))
        {
            builder = builder.background(r, g, b);
        }

        if let Some((r, g, b)) = get_color("terminal.foreground")
            .or_else(|| get_color("editor.foreground"))
            .or_else(|| get_color("text"))
        {
            builder = builder.foreground(r, g, b);
        }

        let cursor_color = target_theme
            .get("players")
            .and_then(Value::as_array)
            .and_then(|p| p.first())
            .and_then(|p| p.get("cursor"))
            .and_then(Value::as_str)
            .and_then(parse_hex)
            .map(hex_to_rgb)
            .or_else(|| get_color("text.accent"))
            .or_else(|| get_color("terminal.bright_foreground"))
            .or_else(|| get_color("terminal.foreground"));

        if let Some((r, g, b)) = cursor_color {
            builder = builder.cursor(r, g, b);
        }

        let ansi_keys = [
            ("terminal.ansi.black", 0),
            ("terminal.ansi.red", 1),
            ("terminal.ansi.green", 2),
            ("terminal.ansi.yellow", 3),
            ("terminal.ansi.blue", 4),
            ("terminal.ansi.magenta", 5),
            ("terminal.ansi.cyan", 6),
            ("terminal.ansi.white", 7),
            ("terminal.ansi.bright_black", 8),
            ("terminal.ansi.bright_red", 9),
            ("terminal.ansi.bright_green", 10),
            ("terminal.ansi.bright_yellow", 11),
            ("terminal.ansi.bright_blue", 12),
            ("terminal.ansi.bright_magenta", 13),
            ("terminal.ansi.bright_cyan", 14),
            ("terminal.ansi.bright_white", 15),
        ];

        for (key, idx) in ansi_keys {
            if let Some((r, g, b)) = get_color(key) {
                builder = match idx {
                    0 => builder.black(r, g, b),
                    1 => builder.red(r, g, b),
                    2 => builder.green(r, g, b),
                    3 => builder.yellow(r, g, b),
                    4 => builder.blue(r, g, b),
                    5 => builder.magenta(r, g, b),
                    6 => builder.cyan(r, g, b),
                    7 => builder.white(r, g, b),
                    8 => builder.bright_black(r, g, b),
                    9 => builder.bright_red(r, g, b),
                    10 => builder.bright_green(r, g, b),
                    11 => builder.bright_yellow(r, g, b),
                    12 => builder.bright_blue(r, g, b),
                    13 => builder.bright_magenta(r, g, b),
                    14 => builder.bright_cyan(r, g, b),
                    15 => builder.bright_white(r, g, b),
                    _ => builder,
                };
            }
        }

        return builder.build();
    }

    if dark {
        ColorPalette::builder()
            .background(0x01, 0x04, 0x09)
            .foreground(0xf0, 0xf6, 0xfc)
            .cursor(0x44, 0x93, 0xf8)
            .build()
    } else {
        ColorPalette::builder()
            .background(0xff, 0xff, 0xff)
            .foreground(0x1f, 0x23, 0x28)
            .cursor(0x09, 0x69, 0xda)
            .build()
    }
}

/// Load the terminal palette for a scheme and light/dark mode.
pub fn load_palette(scheme: PaletteScheme, dark: bool) -> ColorPalette {
    match scheme {
        PaletteScheme::Shob => load_warp_palette(dark),
        PaletteScheme::Github => load_github_palette(dark),
        PaletteScheme::MidnightGarden if dark => {
            terminal::ColorPaletteBuilder::from_palette(load_warp_palette(true))
                .background(0x00, 0x15, 0x36)
                .foreground(0xea, 0xf3, 0xff)
                .cursor(0x39, 0xa8, 0xf5)
                // Some agent CLIs paint their panels with this fixed dark surface.
                .background_override((0x0f, 0x12, 0x16), (0x0a, 0x21, 0x47))
                .build()
        }
        PaletteScheme::MidnightGarden => load_warp_palette(false),
        scheme => imported::lookup(scheme, dark)
            .expect("imported scheme has a palette")
            .terminal(),
    }
}

/// Warp's default terminal palette, transcribed from the open-source Warp
/// repository (`app/src/themes/default_themes.rs`).
///
/// Dark: background `#050505`, foreground `#FFFFFF`, cursor/accent `#19AAD8`.
/// Light: background `#FFFFFF`, foreground `#111111`, accent `#00C2FF`.
/// The 16 ANSI colors match Warp's `DARK_MODE_*` / `LIGHT_MODE_*` constants.
pub fn load_warp_palette(dark: bool) -> ColorPalette {
    let builder = ColorPalette::builder();
    if dark {
        builder
            .background(0x05, 0x05, 0x05)
            .foreground(0xff, 0xff, 0xff)
            .cursor(0x19, 0xaa, 0xd8)
            // normal
            .black(0x61, 0x61, 0x61)
            .red(0xff, 0x82, 0x72)
            .green(0xb4, 0xfa, 0x72)
            .yellow(0xfe, 0xfd, 0xc2)
            .blue(0xa5, 0xd5, 0xfe)
            .magenta(0xff, 0x8f, 0xfd)
            .cyan(0xd0, 0xd1, 0xfe)
            .white(0xf1, 0xf1, 0xf1)
            // bright
            .bright_black(0x8e, 0x8e, 0x8e)
            .bright_red(0xff, 0xc4, 0xbd)
            .bright_green(0xd6, 0xfc, 0xb9)
            .bright_yellow(0xfe, 0xfd, 0xd5)
            .bright_blue(0xc1, 0xe3, 0xfe)
            .bright_magenta(0xff, 0xb1, 0xfe)
            .bright_cyan(0xe5, 0xe6, 0xfe)
            .bright_white(0xfe, 0xff, 0xff)
            .build()
    } else {
        builder
            .background(0xff, 0xff, 0xff)
            .foreground(0x11, 0x11, 0x11)
            .cursor(0x00, 0xc2, 0xff)
            // normal
            .black(0x21, 0x21, 0x21)
            .red(0xc3, 0x07, 0x71)
            .green(0x10, 0xa7, 0x78)
            .yellow(0xa8, 0x9c, 0x14)
            .blue(0x00, 0x8e, 0xc4)
            .magenta(0x52, 0x3c, 0x79)
            .cyan(0x20, 0xa5, 0xba)
            .white(0xe0, 0xe0, 0xe0)
            // bright
            .bright_black(0x21, 0x21, 0x21)
            .bright_red(0xfb, 0x00, 0x7a)
            .bright_green(0x5f, 0xd7, 0xaf)
            .bright_yellow(0xf3, 0xe4, 0x30)
            .bright_blue(0x20, 0xbb, 0xfc)
            .bright_magenta(0x68, 0x55, 0xde)
            .bright_cyan(0x4f, 0xb8, 0xcc)
            .bright_white(0xf1, 0xf1, 0xf1)
            .build()
    }
}
