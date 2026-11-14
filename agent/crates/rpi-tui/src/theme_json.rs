//! Theme JSON parsing: pi `ThemeJson` theme files → [`ThemeColors`].
//!
//! Covers the published pi theme schema (`theme.schema.json` in
//! badlogic/pi-mono): `$schema` / `name` / `appearance` / `vars` / `colors` /
//! `export`, with color values as `#rgb`/`#rrggbb` hex, `oklch()`/`okhsl()`
//! functions, 0-255 palette indices, variable references, or `""` for the
//! terminal default.
//!
//! Known v1 divergences from upstream, kept intentional:
//! - Tokens without a [`ThemeColors`] consumer in rpi-tui yet (`scrollbar*`,
//!   `searchMatch*`, `userMessageText`, `syntax*`, `thinkingOff`…`thinkingMax`)
//!   are parsed and validated so pi-authored files load unchanged, but do not
//!   affect rendering. rpi's syntax highlighting and thinking-level borders
//!   use their own palettes.
//! - `export` colors are HTML-export-only upstream and are ignored here.
//! - Root keys and color tokens are validated strictly (mirrors the upstream
//!   schema), so typos surface as errors instead of silent fallbacks.
//!
//! Missing tokens fall back to [`ThemeColors::default`]: pi allows partial
//! overrides, so a file may provide any subset of `colors`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::LazyLock;

use serde_json::Value;

use crate::oklab::{okhsl_to_rgb, oklch_to_rgb};
use crate::theme::{Color, Theme, ThemeColors};

/// Every color token of the pi theme schema, in schema order, paired with the
/// [`ThemeColors`] field it feeds (`None` = no rendering consumer yet, see the
/// module docs).
type ColorSlot = fn(&mut ThemeColors, Color);

const TOKENS: &[(&str, Option<ColorSlot>)] = &[
    ("accent", Some(|c, v| c.accent = v)),
    ("border", Some(|c, v| c.border = v)),
    ("borderAccent", Some(|c, v| c.border_accent = v)),
    ("borderMuted", Some(|c, v| c.border_muted = v)),
    ("success", Some(|c, v| c.success = v)),
    ("error", Some(|c, v| c.error = v)),
    ("warning", Some(|c, v| c.warning = v)),
    ("muted", Some(|c, v| c.muted = v)),
    ("dim", Some(|c, v| c.dim = v)),
    ("text", Some(|c, v| c.text = v)),
    ("thinkingText", Some(|c, v| c.thinking_text = v)),
    ("scrollbarTrack", None),
    ("scrollbarThumb", None),
    ("selectedBg", Some(|c, v| c.selection = v)),
    ("searchMatchBg", None),
    ("searchMatchText", None),
    ("userMessageBg", Some(|c, v| c.surface = v)),
    ("userMessageText", None),
    ("customMessageBg", Some(|c, v| c.custom_message_bg = v)),
    ("customMessageText", Some(|c, v| c.custom_message_text = v)),
    (
        "customMessageLabel",
        Some(|c, v| c.custom_message_label = v),
    ),
    ("toolPendingBg", Some(|c, v| c.tool_pending_bg = v)),
    ("toolSuccessBg", Some(|c, v| c.tool_success_bg = v)),
    ("toolErrorBg", Some(|c, v| c.tool_error_bg = v)),
    ("toolTitle", Some(|c, v| c.tool_title = v)),
    ("toolOutput", Some(|c, v| c.tool_output = v)),
    ("mdHeading", Some(|c, v| c.md_heading = v)),
    ("mdLink", Some(|c, v| c.md_link = v)),
    ("mdLinkUrl", Some(|c, v| c.md_link_url = v)),
    ("mdCode", Some(|c, v| c.md_code = v)),
    ("mdCodeBlock", Some(|c, v| c.md_code_block = v)),
    ("mdCodeBlockBorder", Some(|c, v| c.md_code_block_border = v)),
    ("mdQuote", Some(|c, v| c.md_quote = v)),
    ("mdQuoteBorder", Some(|c, v| c.md_quote_border = v)),
    ("mdHr", Some(|c, v| c.md_hr = v)),
    ("mdListBullet", Some(|c, v| c.md_list_bullet = v)),
    ("toolDiffAdded", Some(|c, v| c.tool_diff_added = v)),
    ("toolDiffRemoved", Some(|c, v| c.tool_diff_removed = v)),
    ("toolDiffContext", Some(|c, v| c.tool_diff_context = v)),
    ("syntaxComment", None),
    ("syntaxKeyword", None),
    ("syntaxFunction", None),
    ("syntaxVariable", None),
    ("syntaxString", None),
    ("syntaxNumber", None),
    ("syntaxType", None),
    ("syntaxOperator", None),
    ("syntaxPunctuation", None),
    ("thinkingOff", None),
    ("thinkingMinimal", None),
    ("thinkingLow", None),
    ("thinkingMedium", None),
    ("thinkingHigh", None),
    ("thinkingXhigh", None),
    ("thinkingMax", None),
    ("bashMode", Some(|c, v| c.bash_mode = v)),
];

fn token_slot(token: &str) -> bool {
    TOKENS.iter().any(|(name, _)| *name == token)
}

/// A color value as written in a theme file (schema `ColorValue`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonColorValue {
    /// 256-color palette index.
    Indexed(u8),
    /// Hex, `oklch()`/`okhsl()`, variable reference, or `""` (terminal default).
    Text(String),
}

impl JsonColorValue {
    fn from_json(field: &str, value: &Value) -> Result<Self, ThemeJsonError> {
        match value {
            Value::Number(number) => {
                let index = number.as_u64().ok_or_else(|| {
                    ThemeJsonError::new(format!(
                        "{field}: color index must be an integer 0-255, got {number}"
                    ))
                })?;
                let index = u8::try_from(index).map_err(|_| {
                    ThemeJsonError::new(format!(
                        "{field}: color index must be an integer 0-255, got {number}"
                    ))
                })?;
                Ok(JsonColorValue::Indexed(index))
            }
            Value::String(text) => Ok(JsonColorValue::Text(text.clone())),
            other => Err(ThemeJsonError::new(format!(
                "{field}: color value must be a string or 0-255 integer, got {other}"
            ))),
        }
    }
}

/// Error from parsing or applying a theme file. Messages include the field
/// name and the offending value so users can fix their JSON directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeJsonError {
    message: String,
}

impl ThemeJsonError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Prefix the message with the color token it was raised for.
    fn with_token(self, token: &str) -> Self {
        Self {
            message: format!("colors.{token}: {}", self.message),
        }
    }
}

impl fmt::Display for ThemeJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ThemeJsonError {}

/// A parsed pi theme file (`ThemeJson`).
#[derive(Debug, Clone)]
pub struct ThemeJson {
    /// Theme name (`name`, required, must not contain `/`).
    pub name: String,
    /// Reusable color variables (`vars`).
    pub vars: BTreeMap<String, JsonColorValue>,
    /// Color tokens (`colors`), restricted to known pi tokens.
    pub colors: BTreeMap<String, JsonColorValue>,
}

impl ThemeJson {
    /// Parse a theme file from its JSON text.
    pub fn parse(input: &str) -> Result<Self, ThemeJsonError> {
        let root: Value = serde_json::from_str(input)
            .map_err(|e| ThemeJsonError::new(format!("invalid theme JSON: {e}")))?;
        let obj = root
            .as_object()
            .ok_or_else(|| ThemeJsonError::new("theme JSON must be an object"))?;

        for key in obj.keys() {
            if !matches!(
                key.as_str(),
                "$schema" | "name" | "appearance" | "vars" | "colors" | "export"
            ) {
                return Err(ThemeJsonError::new(format!("unknown theme key \"{key}\"")));
            }
        }

        let name = obj
            .get("name")
            .ok_or_else(|| ThemeJsonError::new("theme JSON is missing required field \"name\""))?
            .as_str()
            .ok_or_else(|| ThemeJsonError::new("\"name\" must be a string"))?;
        if name.is_empty() {
            return Err(ThemeJsonError::new("\"name\" must not be empty"));
        }
        if name.contains('/') {
            return Err(ThemeJsonError::new(
                "\"name\" must not contain '/' (reserved for light/dark settings)",
            ));
        }

        if let Some(appearance) = obj.get("appearance") {
            match appearance.as_str() {
                Some("dark") | Some("light") => {}
                _ => {
                    return Err(ThemeJsonError::new(format!(
                        "\"appearance\" must be \"dark\" or \"light\", got {appearance}"
                    )))
                }
            }
        }

        let vars = match obj.get("vars") {
            None => BTreeMap::new(),
            Some(Value::Object(entries)) => {
                let mut vars = BTreeMap::new();
                for (key, value) in entries {
                    vars.insert(
                        key.clone(),
                        JsonColorValue::from_json(&format!("vars.{key}"), value)?,
                    );
                }
                vars
            }
            Some(other) => {
                return Err(ThemeJsonError::new(format!(
                    "\"vars\" must be an object, got {other}"
                )))
            }
        };

        let colors_json = obj
            .get("colors")
            .ok_or_else(|| ThemeJsonError::new("theme JSON is missing required field \"colors\""))?
            .as_object()
            .ok_or_else(|| ThemeJsonError::new("\"colors\" must be an object"))?;
        let mut colors = BTreeMap::new();
        for (key, value) in colors_json {
            if !token_slot(key) {
                return Err(ThemeJsonError::new(format!(
                    "unknown theme color token \"{key}\""
                )));
            }
            colors.insert(
                key.clone(),
                JsonColorValue::from_json(&format!("colors.{key}"), value)?,
            );
        }

        if let Some(export) = obj.get("export") {
            // HTML-export-only upstream; validate the key set but ignore values.
            let entries = export
                .as_object()
                .ok_or_else(|| ThemeJsonError::new("\"export\" must be an object"))?;
            for key in entries.keys() {
                if !matches!(key.as_str(), "pageBg" | "cardBg" | "infoBg") {
                    return Err(ThemeJsonError::new(format!(
                        "unknown export color \"{key}\""
                    )));
                }
            }
        }

        Ok(Self {
            name: name.to_string(),
            vars,
            colors,
        })
    }

    /// Read and parse a theme file from disk.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ThemeJsonError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| {
            ThemeJsonError::new(format!("cannot read theme file {}: {e}", path.display()))
        })?;
        Self::parse(&text)
    }

    /// Resolve one color token to its concrete [`Color`], applying `vars`.
    ///
    /// `Ok(None)` when the file omits the token (caller falls back to the
    /// default palette); `Err` for unknown tokens or invalid values.
    pub fn resolved_color(&self, token: &str) -> Result<Option<Color>, ThemeJsonError> {
        if !token_slot(token) {
            return Err(ThemeJsonError::new(format!(
                "unknown theme color token \"{token}\""
            )));
        }
        match self.colors.get(token) {
            None => Ok(None),
            Some(value) => self
                .resolve_value(value)
                .map(Some)
                .map_err(|e| e.with_token(token)),
        }
    }

    /// Convert into [`ThemeColors`], falling missing tokens back to the
    /// default palette.
    pub fn to_theme_colors(&self) -> Result<ThemeColors, ThemeJsonError> {
        let mut colors = ThemeColors::default();
        // Validate every supplied known token even when it has no rendering
        // slot yet; only tokens with a slot are written through.
        for (token, value) in &self.colors {
            let color = self.resolve_value(value).map_err(|e| e.with_token(token))?;
            if let Some(slot) = TOKENS
                .iter()
                .find(|(name, _)| name == token)
                .and_then(|(_, slot)| *slot)
            {
                slot(&mut colors, color);
            }
        }
        Ok(colors)
    }

    /// Convert into a complete [`Theme`] (spacing/border styles stay default).
    pub fn to_theme(&self) -> Result<Theme, ThemeJsonError> {
        Ok(Theme {
            colors: self.to_theme_colors()?,
            ..Default::default()
        })
    }

    /// Resolve one value through `vars` to a concrete [`Color`].
    fn resolve_value(&self, value: &JsonColorValue) -> Result<Color, ThemeJsonError> {
        let mut visited = BTreeSet::new();
        let mut current = value;
        loop {
            match current {
                JsonColorValue::Indexed(index) => return Ok(Color::Ansi256(*index)),
                JsonColorValue::Text(text) => {
                    if text.is_empty() {
                        return Ok(Color::Default);
                    }
                    if text.starts_with('#') || is_color_function(text) {
                        return parse_color_text(text);
                    }
                    if !visited.insert(text.as_str()) {
                        return Err(ThemeJsonError::new(format!(
                            "circular variable reference detected: \"{text}\""
                        )));
                    }
                    current = self.vars.get(text).ok_or_else(|| {
                        ThemeJsonError::new(format!("variable reference not found: \"{text}\""))
                    })?;
                }
            }
        }
    }
}

/// `oklch(`/`okhsl(` prefix test, case-insensitive (upstream
/// `/^ok(lch|hsl)\(/i` gate before variable lookup).
fn is_color_function(text: &str) -> bool {
    let prefix = text.get(..6);
    matches!(prefix, Some(p) if p.eq_ignore_ascii_case("oklch(") || p.eq_ignore_ascii_case("okhsl("))
}

const NUMBER_PATTERN: &str = r"[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?";

static OKLCH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(&format!(
        r"(?i)^oklch\(\s*({n})(%)?\s+({n})\s+({n})(?:deg)?\s*\)$",
        n = NUMBER_PATTERN
    ))
    .expect("oklch pattern")
});

static OKHSL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(&format!(
        r"(?i)^okhsl\(\s*({n})(?:deg)?\s+({n})(%)?\s+({n})(%)?\s*\)$",
        n = NUMBER_PATTERN
    ))
    .expect("okhsl pattern")
});

fn parse_number(field: &str, text: &str) -> Result<f64, ThemeJsonError> {
    text.parse::<f64>()
        .map_err(|_| ThemeJsonError::new(format!("{field}: invalid number \"{text}\"")))
}

fn scaled_number(field: &str, text: &str, percent: Option<&str>) -> Result<f64, ThemeJsonError> {
    let value = parse_number(field, text)?;
    Ok(if percent.is_some() {
        value / 100.0
    } else {
        value
    })
}

/// Parse a concrete color spelling: hex, `oklch()`, or `okhsl()`.
fn parse_color_text(value: &str) -> Result<Color, ThemeJsonError> {
    if let Some(hex) = value.strip_prefix('#') {
        let digits = match hex.len() {
            3 => hex.chars().flat_map(|c| [c, c]).collect::<String>(),
            6 => hex.to_string(),
            _ => {
                return Err(ThemeJsonError::new(format!(
                    "invalid color value: \"{value}\""
                )))
            }
        };
        if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ThemeJsonError::new(format!(
                "invalid color value: \"{value}\""
            )));
        }
        let channel = |range: std::ops::Range<usize>| {
            u8::from_str_radix(&digits[range], 16).expect("validated hex digits")
        };
        return Ok(Color::Rgb(channel(0..2), channel(2..4), channel(4..6)));
    }

    if let Some(caps) = OKLCH_RE.captures(value) {
        let l = scaled_number("oklch lightness", &caps[1], caps.get(2).map(|m| m.as_str()))?;
        let c = parse_number("oklch chroma", &caps[3])?;
        if c < 0.0 {
            return Err(ThemeJsonError::new(format!(
                "oklch chroma must not be negative: {c}"
            )));
        }
        let h = parse_number("oklch hue", &caps[4])?;
        return Ok(rgb_color(oklch_to_rgb(l, c, h)));
    }

    if let Some(caps) = OKHSL_RE.captures(value) {
        let h = parse_number("okhsl hue", &caps[1])?;
        let s = scaled_number(
            "okhsl saturation",
            &caps[2],
            caps.get(3).map(|m| m.as_str()),
        )?;
        if !(0.0..=1.0).contains(&s) {
            return Err(ThemeJsonError::new(format!(
                "okhsl saturation must be between 0 and 1: {s}"
            )));
        }
        let l = scaled_number("okhsl lightness", &caps[4], caps.get(5).map(|m| m.as_str()))?;
        if !(0.0..=1.0).contains(&l) {
            return Err(ThemeJsonError::new(format!(
                "okhsl lightness must be between 0 and 1: {l}"
            )));
        }
        return Ok(rgb_color(okhsl_to_rgb(h, s, l)));
    }

    Err(ThemeJsonError::new(format!(
        "invalid color value: \"{value}\""
    )))
}

fn rgb_color(rgb: [u8; 3]) -> Color {
    Color::Rgb(rgb[0], rgb[1], rgb[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_short_and_long_forms() {
        assert_eq!(parse_color_text("#8abeb7"), Ok(Color::Rgb(138, 190, 183)));
        assert_eq!(parse_color_text("#8AB"), Ok(Color::Rgb(136, 170, 187)));
        assert_eq!(parse_color_text("#abc"), parse_color_text("#aabbcc"));
    }

    #[test]
    fn indexed_values_become_ansi256() {
        let json: Value = serde_json::from_str("196").unwrap();
        let value = JsonColorValue::from_json("colors.accent", &json).unwrap();
        let theme = ThemeJson {
            name: "t".into(),
            vars: BTreeMap::new(),
            colors: [("accent".to_string(), value)].into_iter().collect(),
        };
        assert!(matches!(
            theme.to_theme_colors().unwrap().accent,
            Color::Ansi256(196)
        ));
    }

    #[test]
    fn fractional_index_rejected_with_field_and_value() {
        let json: Value = serde_json::from_str("12.5").unwrap();
        let err = JsonColorValue::from_json("colors.accent", &json).unwrap_err();
        assert!(err.to_string().contains("colors.accent"), "{err}");
        assert!(err.to_string().contains("12.5"), "{err}");

        let json: Value = serde_json::from_str("300").unwrap();
        let err = JsonColorValue::from_json("colors.accent", &json).unwrap_err();
        assert!(err.to_string().contains("300"), "{err}");
    }

    #[test]
    fn empty_string_is_terminal_default() {
        let theme = ThemeJson::parse(r#"{"name":"t","colors":{"mdCode":""}}"#).unwrap();
        assert_eq!(theme.to_theme_colors().unwrap().md_code, Color::Default);
    }

    #[test]
    fn vars_chain_through_vars_to_hex() {
        let theme = ThemeJson::parse(
            r##"{"name":"t","vars":{"a":"#ff0000","b":"a","c":"b"},"colors":{"accent":"c"}}"##,
        )
        .unwrap();
        assert_eq!(
            theme.to_theme_colors().unwrap().accent,
            Color::Rgb(255, 0, 0)
        );
    }

    #[test]
    fn vars_to_indexed_value() {
        let theme =
            ThemeJson::parse(r#"{"name":"t","vars":{"v":42},"colors":{"accent":"v"}}"#).unwrap();
        assert!(matches!(
            theme.to_theme_colors().unwrap().accent,
            Color::Ansi256(42)
        ));
    }

    #[test]
    fn circular_var_reference_is_an_error() {
        let theme =
            ThemeJson::parse(r#"{"name":"t","vars":{"a":"b","b":"a"},"colors":{"accent":"a"}}"#)
                .unwrap();
        let err = theme.to_theme_colors().unwrap_err();
        assert!(err.to_string().contains("colors.accent"), "{err}");
        assert!(err.to_string().contains("circular"), "{err}");
    }

    #[test]
    fn unknown_var_reference_names_the_token_and_variable() {
        let theme = ThemeJson::parse(r#"{"name":"t","colors":{"mdCode":"violet"}}"#).unwrap();
        let err = theme.to_theme_colors().unwrap_err();
        assert!(err.to_string().contains("colors.mdCode"), "{err}");
        assert!(err.to_string().contains("violet"), "{err}");
    }

    #[test]
    fn invalid_color_value_names_token_and_value() {
        let theme = ThemeJson::parse(r#"{"name":"t","colors":{"mdCode":"not-a-color"}}"#).unwrap();
        let err = theme.to_theme_colors().unwrap_err();
        assert!(err.to_string().contains("colors.mdCode"), "{err}");
        assert!(err.to_string().contains("not-a-color"), "{err}");
    }

    #[test]
    fn bad_hex_length_is_rejected() {
        assert!(parse_color_text("#12345").is_err());
        assert!(parse_color_text("#12345g").is_err());
    }

    #[test]
    fn unknown_token_names_the_token() {
        let err = ThemeJson::parse(r##"{"name":"t","colors":{"mdHeadingg":"#fff"}}"##).unwrap_err();
        assert!(err.to_string().contains("mdHeadingg"), "{err}");
    }

    // review_pr198: slotless tokens (no ThemeColors consumer) must still be
    // validated when the file is loaded, not only via resolved_color().
    #[test]
    fn review_pr198_slotless_invalid_value_fails_load() {
        let theme = ThemeJson::parse(r##"{"name":"t","colors":{"syntaxComment":"#gggggg"}}"##)
            .expect("probe input must parse");
        let err = theme.to_theme().unwrap_err();
        assert!(err.to_string().contains("colors.syntaxComment"), "{err}");
        assert!(err.to_string().contains("#gggggg"), "{err}");
    }

    #[test]
    fn review_pr198_slotless_var_cycle_fails_load() {
        let theme = ThemeJson::parse(
            r#"{"name":"t","vars":{"a":"b","b":"a"},"colors":{"thinkingHigh":"a"}}"#,
        )
        .expect("probe input must parse");
        let err = theme.to_theme().unwrap_err();
        assert!(err.to_string().contains("colors.thinkingHigh"), "{err}");
        assert!(err.to_string().contains("circular"), "{err}");
    }

    #[test]
    fn review_pr198_valid_slotless_token_still_loads() {
        let theme = ThemeJson::parse(
            r##"{"name":"t","colors":{"syntaxComment":"#8abeb7","accent":"#1f9"}}"##,
        )
        .unwrap();
        let colors = theme.to_theme_colors().unwrap();
        assert_eq!(colors.accent, Color::Rgb(17, 255, 153));
    }

    #[test]
    fn missing_name_is_rejected() {
        assert!(ThemeJson::parse(r#"{"colors":{}}"#).is_err());
        assert!(ThemeJson::parse(r#"{"name":"a/b","colors":{}}"#).is_err());
    }

    #[test]
    fn missing_colors_is_rejected() {
        assert!(ThemeJson::parse(r#"{"name":"t"}"#).is_err());
    }

    #[test]
    fn unknown_root_key_is_rejected() {
        let err = ThemeJson::parse(r#"{"name":"t","colors":{},"bogus":1}"#).unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn export_keys_accepted_and_ignored() {
        let theme = ThemeJson::parse(
            r##"{"name":"t","colors":{},"export":{"pageBg":"#fff","cardBg":"#eee","infoBg":"#ddd"}}"##,
        )
        .unwrap();
        assert_eq!(theme.name, "t");
        let err =
            ThemeJson::parse(r##"{"name":"t","colors":{},"export":{"nope":"#fff"}}"##).unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn appearance_must_be_dark_or_light() {
        assert!(ThemeJson::parse(r#"{"name":"t","appearance":"dark","colors":{}}"#).is_ok());
        let err =
            ThemeJson::parse(r#"{"name":"t","appearance":"midnight","colors":{}}"#).unwrap_err();
        assert!(err.to_string().contains("midnight"), "{err}");
    }

    #[test]
    fn okhsl_percent_and_fraction_forms_agree() {
        let percent = parse_color_text("okhsl(295 50% 67%)").unwrap();
        let fraction = parse_color_text("okhsl(295 0.5 0.67)").unwrap();
        assert_eq!(percent, fraction);
        // Pin the exact channels so the okhsl pipeline stays anchored.
        assert_eq!(percent, Color::Rgb(167, 152, 215));
    }

    #[test]
    fn okhsl_out_of_range_saturations_rejected() {
        assert!(parse_color_text("okhsl(0 1.2 0.5)").is_err());
        assert!(parse_color_text("okhsl(0 120% 0.5)").is_err());
        assert!(parse_color_text("okhsl(0 0.5 1.5)").is_err());
        let theme =
            ThemeJson::parse(r#"{"name":"t","colors":{"accent":"okhsl(0 1.2 0.5)"}}"#).unwrap();
        let err = theme.to_theme_colors().unwrap_err();
        assert!(err.to_string().contains("colors.accent"), "{err}");
    }

    #[test]
    fn oklch_negative_chroma_rejected() {
        assert!(parse_color_text("oklch(0.5 -0.1 30)").is_err());
    }

    #[test]
    fn oklch_lightness_percent_form() {
        assert_eq!(
            parse_color_text("oklch(50% 0.1 30)"),
            parse_color_text("oklch(0.5 0.1 30)")
        );
    }

    #[test]
    fn partial_file_falls_back_to_default_palette() {
        let theme = ThemeJson::parse(r##"{"name":"half","colors":{"accent":"#ff0000"}}"##).unwrap();
        let colors = theme.to_theme_colors().unwrap();
        assert_eq!(colors.accent, Color::Rgb(255, 0, 0));
        let defaults = ThemeColors::default();
        assert_eq!(colors.md_heading, defaults.md_heading);
        assert_eq!(colors.border, defaults.border);
        assert_eq!(colors.tool_diff_added, defaults.tool_diff_added);
    }

    #[test]
    fn token_table_is_consistent() {
        // 57 pi tokens, no duplicates; slots point at distinct fields is not
        // checkable via fn pointers, but token uniqueness and known names are.
        assert_eq!(TOKENS.len(), 56);
        for (name, _) in TOKENS {
            assert!(!name.is_empty());
            assert!(!name.contains('/'));
        }
        let unique = TOKENS
            .iter()
            .map(|(name, _)| *name)
            .collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), TOKENS.len());
    }
}
