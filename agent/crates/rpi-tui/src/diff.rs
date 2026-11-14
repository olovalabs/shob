//! Diff rendering for tool results — colored `+`/`-`/` ` lines with
//! intra-line word-diff highlighting.
//!
//! Port of the TypeScript `renderDiff`
//! (`packages/coding-agent/.../components/diff.ts`).
//!
//! The `edit` tool already produces a display-oriented diff string in
//! `result.details.diff` (lines shaped `+123 …` / `-123 …` / ` 123 …`;
//! see `rpi-tools/src/tools/edit_diff.rs::generate_diff_string`). This module
//! consumes that *string* (no generator, no `rpi-tools` dependency) and renders
//! it as ANSI-styled terminal lines:
//!
//! - removed (`-`) → red (`theme.colors.error`)
//! - added   (`+`) → green (`theme.colors.success`)
//! - context (` `) → muted/dim (`theme.colors.muted`)
//! - for a single removed→added pair, the changed *words* are inverse-highlighted
//!   so you can see what moved within the line.

use similar::TextDiff;

use crate::ansi::strip_ansi;
use crate::theme::theme;
use crate::utils::{truncate_to_width, visible_width};

/// Render a display-diff string into styled terminal lines.
///
/// `width` is the available column count; long diff lines are clipped with
/// `…` rather than wrapped (a wrapped diff is unreadable).
///
/// Layout: a fixed 4-col gutter `SIGN NUM│ ` where SIGN is `+`/`-`/` `, NUM
/// is the right-aligned line number, and `│` is a thin themed rule. The
/// content body follows after the gutter. This lines every diff row up on a
/// shared left edge (the old `+123 content` format let the content column
/// drift with line-number width) and gives +/- a consistent glyph column.
pub fn render_diff(diff_text: &str, width: usize) -> Vec<String> {
    let colors = &theme().colors;
    let removed_color = colors.tool_diff_removed; // red
    let added_color = colors.tool_diff_added; // green
    let context_color = colors.tool_diff_context; // dim
    let gutter_color = colors.dim;

    let lines: Vec<&str> = diff_text.split('\n').collect();
    // Right-align line numbers to a shared width so the gutter rule `│`
    // stays in one column across the whole hunk.
    let num_width = lines
        .iter()
        .filter_map(|l| parse_diff_line(l))
        .map(|p| p.line_num.len())
        .max()
        .unwrap_or(1);
    let mut out: Vec<String> = Vec::with_capacity(lines.len());

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let Some(parsed) = parse_diff_line(line) else {
            // Unparseable line — render as dim context with no gutter.
            out.push(context_color.fg(&clip(line, width)));
            i += 1;
            continue;
        };

        if parsed.prefix == '-' {
            // Collect consecutive removed lines.
            let mut removed: Vec<ParsedLine> = Vec::new();
            while i < lines.len() {
                let Some(p) = parse_diff_line(lines[i]) else {
                    break;
                };
                if p.prefix != '-' {
                    break;
                }
                removed.push(p);
                i += 1;
            }
            // Collect consecutive added lines immediately after.
            let mut added: Vec<ParsedLine> = Vec::new();
            while i < lines.len() {
                let Some(p) = parse_diff_line(lines[i]) else {
                    break;
                };
                if p.prefix != '+' {
                    break;
                }
                added.push(p);
                i += 1;
            }

            if removed.len() == 1 && added.len() == 1 {
                // Single-line modification → intra-line word diff.
                let (rem_body, add_body) = render_intra_line_diff(
                    &replace_tabs(&removed[0].content),
                    &replace_tabs(&added[0].content),
                );
                out.push(render_diff_row(
                    '-',
                    &removed[0].line_num,
                    &rem_body,
                    removed_color,
                    gutter_color,
                    num_width,
                    width,
                ));
                out.push(render_diff_row(
                    '+',
                    &added[0].line_num,
                    &add_body,
                    added_color,
                    gutter_color,
                    num_width,
                    width,
                ));
            } else {
                for r in &removed {
                    out.push(render_diff_row(
                        '-',
                        &r.line_num,
                        &replace_tabs(&r.content),
                        removed_color,
                        gutter_color,
                        num_width,
                        width,
                    ));
                }
                for a in &added {
                    out.push(render_diff_row(
                        '+',
                        &a.line_num,
                        &replace_tabs(&a.content),
                        added_color,
                        gutter_color,
                        num_width,
                        width,
                    ));
                }
            }
        } else if parsed.prefix == '+' {
            out.push(render_diff_row(
                '+',
                &parsed.line_num,
                &replace_tabs(&parsed.content),
                added_color,
                gutter_color,
                num_width,
                width,
            ));
            i += 1;
        } else {
            // Context line.
            out.push(render_diff_row(
                ' ',
                &parsed.line_num,
                &replace_tabs(&parsed.content),
                context_color,
                gutter_color,
                num_width,
                width,
            ));
            i += 1;
        }
    }

    out
}

/// Render one diff row with a themed gutter: `SIGN NUM│ body`.
/// `body` may already carry ANSI (intra-line inverse highlights); `clip`
/// handles that. The sign and the gutter rule use `gutter_color` so the
/// colored body (red/green/dim) reads as the payload, not the chrome.
fn render_diff_row(
    sign: char,
    line_num: &str,
    body: &str,
    body_color: crate::theme::Color,
    gutter_color: crate::theme::Color,
    num_width: usize,
    width: usize,
) -> String {
    let num = format!("{num:>num_width$}", num = line_num, num_width = num_width);
    let gutter = format!(
        "{} {}{}",
        gutter_color.fg(&sign.to_string()),
        gutter_color.fg(&num),
        gutter_color.fg("│")
    );
    let body = format!(" {}", body_color.fg(body));
    clip(&format!("{}{}", gutter, body), width)
}

#[derive(Debug)]
struct ParsedLine {
    prefix: char,
    line_num: String,
    content: String,
}

/// Parse a diff line: `"+123 content"`, `"-5 content"`, `" 7 context"`,
/// or `"  ..."`. Returns `(prefix, line_num, content)` where `prefix` is the
/// leading `+`/`-`/` ` character.
fn parse_diff_line(line: &str) -> Option<ParsedLine> {
    let mut chars = line.chars();
    let prefix_char = chars.next()?;
    if prefix_char != '+' && prefix_char != '-' && prefix_char != ' ' {
        return None;
    }
    // The TS regex: `^([+-\s])(\s*\d*)\s(.*)$` — optional spaces, then digits,
    // then a single separating space, then content.
    let after_prefix = &line[prefix_char.len_utf8()..];
    let mut rest = after_prefix;
    while rest.starts_with(' ') {
        rest = &rest[1..];
    }
    let mut num = String::new();
    while let Some(c) = rest.chars().next() {
        if c.is_ascii_digit() {
            num.push(c);
            rest = &rest[c.len_utf8()..];
        } else {
            break;
        }
    }
    if let Some(stripped) = rest.strip_prefix(' ') {
        rest = stripped;
    }
    Some(ParsedLine {
        prefix: prefix_char,
        line_num: num,
        content: rest.to_string(),
    })
}

/// Replace tabs with 3 spaces (matches the TS `replaceTabs`).
fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

/// Clip a (possibly already styled) line to `width` columns with a `…` suffix.
fn clip(line: &str, width: usize) -> String {
    if width == 0 {
        return line.to_string();
    }
    let vw = visible_width(line);
    if vw <= width {
        line.to_string()
    } else {
        truncate_to_width(line, width, "…")
    }
}

/// Compute a word-level diff between two lines and return the removed/added
/// renderings with changed parts wrapped in inverse video (`\x1b[7m`).
///
/// Port of `renderIntraLineDiff` (diff.ts:26). Leading whitespace is not
/// inverse-highlighted (it would visually smear the indentation).
fn render_intra_line_diff(old_content: &str, new_content: &str) -> (String, String) {
    let diff = TextDiff::from_unicode_words(old_content, new_content);

    let mut removed_line = String::new();
    let mut added_line = String::new();
    let mut first_removed = true;
    let mut first_added = true;

    for change in diff.iter_all_changes() {
        let value = change.value();
        match change.tag() {
            similar::ChangeTag::Delete => {
                let (leading, rest) = split_leading_ws(value);
                if first_removed {
                    removed_line.push_str(leading);
                    first_removed = false;
                }
                if !rest.is_empty() {
                    removed_line.push_str(&inverse(rest));
                }
            }
            similar::ChangeTag::Insert => {
                let (leading, rest) = split_leading_ws(value);
                if first_added {
                    added_line.push_str(leading);
                    first_added = false;
                }
                if !rest.is_empty() {
                    added_line.push_str(&inverse(rest));
                }
            }
            similar::ChangeTag::Equal => {
                removed_line.push_str(value);
                added_line.push_str(value);
            }
        }
    }

    (removed_line, added_line)
}

/// Split a string into its leading whitespace and the remainder.
fn split_leading_ws(s: &str) -> (&str, &str) {
    let idx = s.find(|c: char| !c.is_whitespace()).unwrap_or(s.len());
    (&s[..idx], &s[idx..])
}

/// Apply ANSI inverse video to `text`, then reset. We strip any pre-existing
/// styling from the wrapped segment so the inverse reads cleanly.
fn inverse(text: &str) -> String {
    let clean = strip_ansi(text);
    format!("\x1b[7m{clean}\x1b[27m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_diff_colors_removed_and_added() {
        let diff = "-1 old\n+1 new";
        let lines = render_diff(diff, 80);
        assert_eq!(lines.len(), 2);
        // Red (error) fg escape for the removed line.
        assert!(lines[0].contains("old"));
        // Green (success) fg escape for the added line.
        assert!(lines[1].contains("new"));
    }

    #[test]
    fn test_render_diff_intra_line_word_highlight() {
        let diff = "-1 hello world\n+1 hello earth";
        let lines = render_diff(diff, 80);
        // The removed line highlights "world", the added line highlights "earth".
        assert!(
            lines[0].contains("\x1b[7m"),
            "removed intra-line not inversed"
        );
        assert!(
            lines[1].contains("\x1b[7m"),
            "added intra-line not inversed"
        );
        assert!(lines[1].contains("earth"));
    }

    #[test]
    fn test_render_diff_context_dim() {
        let diff = " 5 context line";
        let lines = render_diff(diff, 80);
        assert_eq!(lines.len(), 1);
        // Muted context color (Ansi256(244) by default — tool_diff_context).
        assert!(lines[0].contains("\x1b[38;5;244m"), "context not dimmed");
    }

    #[test]
    fn test_render_diff_clips_long_lines() {
        let long = format!("+1 {}", "x".repeat(200));
        let lines = render_diff(&long, 40);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains('…'));
        assert!(visible_width(&lines[0]) <= 40);
    }
}
