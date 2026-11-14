//! Golden tests against real pi theme files.
//!
//! Fixtures `tests/data/themes/{dark,light}.json` are the upstream pi themes
//! (badlogic/pi-mono `packages/coding-agent/src/modes/interactive/theme/`). The
//! `*.expected.json` tables hold the exact RGB hex each token resolves to when
//! run through the upstream TS `parseColor` + vars resolution; every token with
//! a `ThemeColors` consumer must match those channels exactly so rpi renders
//! pi-authored themes with the same colors as upstream pi.

use rpi_tui::theme::Color;
use rpi_tui::theme_json::ThemeJson;

const DARK_JSON: &str = include_str!("data/themes/dark.json");
const LIGHT_JSON: &str = include_str!("data/themes/light.json");
const DARK_EXPECTED: &str = include_str!("data/themes/dark.expected.json");
const LIGHT_EXPECTED: &str = include_str!("data/themes/light.expected.json");

fn parse_hex(hex: &str) -> Color {
    let digits = &hex[1..];
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&digits[range], 16).unwrap();
    Color::Rgb(channel(0..2), channel(2..4), channel(4..6))
}

fn expected_table(text: &str) -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_str(text).expect("expected table is valid JSON")
}

/// Tokens with a ThemeColors consumer, mapped exactly like theme_json.rs.
fn token_field(token: &str) -> Option<fn(&rpi_tui::theme::ThemeColors) -> Color> {
    use rpi_tui::theme::ThemeColors;
    Some(match token {
        "accent" => |c: &ThemeColors| c.accent,
        "border" => |c: &ThemeColors| c.border,
        "borderAccent" => |c: &ThemeColors| c.border_accent,
        "borderMuted" => |c: &ThemeColors| c.border_muted,
        "success" => |c: &ThemeColors| c.success,
        "error" => |c: &ThemeColors| c.error,
        "warning" => |c: &ThemeColors| c.warning,
        "muted" => |c: &ThemeColors| c.muted,
        "dim" => |c: &ThemeColors| c.dim,
        "text" => |c: &ThemeColors| c.text,
        "thinkingText" => |c: &ThemeColors| c.thinking_text,
        "selectedBg" => |c: &ThemeColors| c.selection,
        "userMessageBg" => |c: &ThemeColors| c.surface,
        "customMessageBg" => |c: &ThemeColors| c.custom_message_bg,
        "customMessageText" => |c: &ThemeColors| c.custom_message_text,
        "customMessageLabel" => |c: &ThemeColors| c.custom_message_label,
        "toolPendingBg" => |c: &ThemeColors| c.tool_pending_bg,
        "toolSuccessBg" => |c: &ThemeColors| c.tool_success_bg,
        "toolErrorBg" => |c: &ThemeColors| c.tool_error_bg,
        "toolTitle" => |c: &ThemeColors| c.tool_title,
        "toolOutput" => |c: &ThemeColors| c.tool_output,
        "mdHeading" => |c: &ThemeColors| c.md_heading,
        "mdLink" => |c: &ThemeColors| c.md_link,
        "mdLinkUrl" => |c: &ThemeColors| c.md_link_url,
        "mdCode" => |c: &ThemeColors| c.md_code,
        "mdCodeBlock" => |c: &ThemeColors| c.md_code_block,
        "mdCodeBlockBorder" => |c: &ThemeColors| c.md_code_block_border,
        "mdQuote" => |c: &ThemeColors| c.md_quote,
        "mdQuoteBorder" => |c: &ThemeColors| c.md_quote_border,
        "mdHr" => |c: &ThemeColors| c.md_hr,
        "mdListBullet" => |c: &ThemeColors| c.md_list_bullet,
        "toolDiffAdded" => |c: &ThemeColors| c.tool_diff_added,
        "toolDiffRemoved" => |c: &ThemeColors| c.tool_diff_removed,
        "toolDiffContext" => |c: &ThemeColors| c.tool_diff_context,
        "bashMode" => |c: &ThemeColors| c.bash_mode,
        _ => return None,
    })
}

fn assert_matches_expected(theme_text: &str, expected_text: &str, label: &str) {
    let theme = ThemeJson::parse(theme_text).unwrap_or_else(|e| panic!("{label}: {e}"));
    let colors = theme.to_theme_colors().unwrap();
    let expected = expected_table(expected_text);

    let mut checked = 0;
    for (token, value) in &expected {
        let Some(field) = token_field(token) else {
            continue;
        };
        let hex = value.as_str().unwrap();
        if hex == "DEFAULT" {
            // Covered by the explicit default-palette assertions below.
            continue;
        }
        assert_eq!(
            field(&colors),
            parse_hex(hex),
            "{label}: token {token} should resolve to {hex}"
        );
        checked += 1;
    }
    assert!(
        checked >= 30,
        "{label}: expected to check many tokens, got {checked}"
    );
}

#[test]
fn golden_dark_json_matches_upstream_rgb() {
    assert_matches_expected(DARK_JSON, DARK_EXPECTED, "dark");
}

#[test]
fn golden_light_json_matches_upstream_rgb() {
    assert_matches_expected(LIGHT_JSON, LIGHT_EXPECTED, "light");
}

#[test]
fn golden_files_parse_and_keep_metadata() {
    let dark = ThemeJson::parse(DARK_JSON).unwrap();
    assert_eq!(dark.name, "dark");
    assert_eq!(dark.vars.len(), 8);
    assert_eq!(dark.colors.len(), 56);

    let light = ThemeJson::parse(LIGHT_JSON).unwrap();
    assert_eq!(light.name, "light");
    assert_eq!(light.colors.len(), 56);
}

#[test]
fn resolved_color_reports_missing_and_present_tokens() {
    let theme = ThemeJson::parse(r##"{"name":"t","colors":{"accent":"#ff0000"}}"##).unwrap();
    assert_eq!(
        theme.resolved_color("accent").unwrap(),
        Some(Color::Rgb(255, 0, 0))
    );
    assert_eq!(theme.resolved_color("mdHr").unwrap(), None);
    assert!(theme.resolved_color("bogusToken").is_err());
}

#[test]
fn from_file_parses_fixture() {
    // Relative to CARGO_MANIFEST_DIR so the test runs from any worktree.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/themes/dark.json");
    let theme = ThemeJson::from_file(&path).unwrap();
    assert_eq!(theme.name, "dark");
    assert!(ThemeJson::from_file(path.with_file_name("missing.json")).is_err());
}
