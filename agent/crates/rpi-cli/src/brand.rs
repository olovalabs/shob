//! The startup brand lockup: the three-bar mark, the `rpi` wordmark, and the
//! Rust crab standing in for the language name.
//!
//! The layout exists as a module rather than inline in the welcome header for
//! one reason: **width must be unambiguous.** The lockup this replaced drew a
//! crab above a box containing `π`. `π` is East-Asian *Ambiguous* — width 1 in
//! most locales and width 2 under a CJK locale — so the box borders walked out
//! of alignment depending on the user's system. The mark here is built from `█`,
//! which is single-width everywhere; the crab is the only wide glyph, so it is
//! the one thing the layout has to account for, and a test pins its width.
//!
//! The mark and the wordmark also supply the terminal window/tab title (see
//! [`window_title`]), so the tab and the startup screen cannot drift apart.

use rpi_tui::{bold as tui_bold, theme as current_theme, Component};

use crate::args::is_truthy_env_flag;

/// Set to `1`/`true`/`yes` to render the lockup without the crab.
pub const NO_EMOJI_ENV: &str = "RPI_NO_EMOJI";

/// The Rust mascot, standing where the word "rust" used to be.
///
/// Emoji render correctly in every terminal we target, but the rest of the UI
/// deliberately avoids them (see the tool status indicator comment in
/// `rpi-tui`), so this one is opt-out rather than mandatory.
pub const CRAB: &str = "🦀";

/// Separator between `rpi` and the crab.
const SEPARATOR: &str = "·";

/// The three-bar mark as a single plain-text row: the lockup's silhouette,
/// scaled into one row.
///
/// `render_lockup` colourises the bars with theme escapes and stacks the bar
/// heights; neither survives into an OSC 2 window title, which must stay
/// escape-free and single-row, so callers that need the mark as text (see
/// [`window_title`]) use this instead.
///
/// Each bar is **one cell** wide (`██` per bar reads as three fat slabs in a
/// tab strip) and its height is a lower-eighth block — one block glyph covers
/// one lockup row, so the 3:5:2 profile of the startup lockup survives intact:
/// the tallest bar is the middle one, the shortest the right one. Block
/// elements have the same width class as the `█` this replaced, so the title
/// still cannot drift on a CJK system the way the old `π` mark did.
pub const MARK: &str = "▃ ▅ ▂";

/// Pure form of the opt-out check, so the logic is testable without mutating the
/// process environment (which is global state and races with parallel tests).
pub(crate) fn crab_enabled_from(value: Option<&str>) -> bool {
    !is_truthy_env_flag(value)
}

fn crab_enabled() -> bool {
    crab_enabled_from(std::env::var(NO_EMOJI_ENV).ok().as_deref())
}

/// The wordmark plus the language mark, in plain text: `rpi · 🦀`, or bare
/// `rpi` when the crab is disabled.
fn plain_name(crab: bool) -> String {
    if crab {
        format!("rpi {SEPARATOR} {CRAB}")
    } else {
        "rpi".to_string()
    }
}

/// The startup lockup flattened into a terminal window/tab title.
///
/// The tab title has one row, so it carries the mark and the wordmark from the
/// lockup; a lower-eighth block per bar (see [`MARK`]) is what keeps the lockup's
/// bar heights readable in that single row. `status` is the activity glyph the
/// caller appends (`"⟳"` in flight, `"↻"` retrying, empty when idle); it goes last
/// so a truncated tab still shows the mark and the name first. Honours
/// [`NO_EMOJI_ENV`] exactly like the lockup, so the two never disagree.
pub fn window_title(status: &str) -> String {
    window_title_from(crab_enabled(), status)
}

/// Pure form of [`window_title`], so the crab opt-out is testable without
/// mutating the process environment (globals race with parallel tests).
fn window_title_from(crab: bool, status: &str) -> String {
    let head = format!("{MARK} {}", plain_name(crab));
    if status.is_empty() {
        head
    } else {
        format!("{head} {status}")
    }
}

/// The five rows of the mark and wordmark. `crab` adds the mascot after the
/// wordmark; without it the wordmark stands alone.
fn render_lockup(crab: bool) -> Vec<String> {
    let c = current_theme().colors;
    let bar = c.dim.fg("██");
    let bar_accent = c.accent.fg("██");

    // Name plus the language mark. Without the crab there is nothing to
    // separate, so the separator goes with it rather than dangling.
    let name = if crab {
        format!(
            "{} {} {CRAB}",
            c.accent.fg(&tui_bold("rpi")),
            c.muted.fg(SEPARATOR)
        )
    } else {
        c.accent.fg(&tui_bold("rpi"))
    };

    vec![
        format!("      {bar_accent}"),
        format!("      {bar_accent}"),
        format!("   {bar} {bar_accent}"),
        format!("   {bar} {bar_accent} {bar}"),
        format!("   {bar} {bar_accent} {bar}   {name}"),
    ]
}

/// The startup lockup. Add one to the welcome header.
pub struct BrandLockup;

impl BrandLockup {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BrandLockup {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for BrandLockup {
    fn render(&self, _width: usize) -> Vec<String> {
        render_lockup(crab_enabled())
    }

    fn invalidate(&self) {
        // Nothing is cached.
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::ansi::{strip_ansi, visible_width};

    fn plain(crab: bool) -> Vec<String> {
        render_lockup(crab).iter().map(|r| strip_ansi(r)).collect()
    }

    #[test]
    fn mark_rows_match_the_site_brand() {
        let rows = plain(true);

        // Three bottom-aligned bars: the tallest in the middle, one row taller
        // than the left bar and two taller than the right one.
        assert_eq!(rows[0], "      ██");
        assert_eq!(rows[1], "      ██");
        assert_eq!(rows[2], "   ██ ██");
        assert_eq!(rows[3], "   ██ ██ ██");

        // The tab-title mark is that same silhouette in one row, so the bar
        // profile has to match bar for bar — one eighth-block per lockup row.
        // Anyone re-tuning the lockup has to re-tune `MARK` with it.
        let rows: Vec<usize> = (0..3)
            .map(|i| {
                let col = 3 + i * 3;
                plain(true)
                    .iter()
                    .filter(|r| r.chars().skip(col).take(2).collect::<String>() == "██")
                    .count()
            })
            .collect();
        assert_eq!(rows, vec![3, 5, 2], "lockup bar profile changed");
        let title: Vec<usize> = MARK
            .chars()
            .filter_map(|c| "▁▂▃▄▅▆▇█".chars().position(|b| b == c).map(|h| h + 1))
            .collect();
        assert_eq!(title, rows, "the tab title must mirror the lockup mark");
    }

    #[test]
    fn crab_stands_in_for_the_language_name() {
        let rows = plain(true);

        // The crab replaces the word "rust" rather than sitting beside it.
        assert_eq!(rows[4], "   ██ ██ ██   rpi · 🦀");

        // The crab is wider than the word it replaces, and the layout must
        // account for exactly that much: the separator plus the emoji.
        let without = visible_width(&plain(false)[4]);
        assert_eq!(
            visible_width(&rows[4]) - without,
            visible_width(" · ") + visible_width(CRAB)
        );
    }

    #[test]
    fn title_flattens_the_lockup_and_keeps_status_last() {
        assert_eq!(window_title_from(true, ""), "▃ ▅ ▂ rpi · 🦀");
        assert_eq!(window_title_from(true, "⟳"), "▃ ▅ ▂ rpi · 🦀 ⟳");
        assert_eq!(window_title_from(true, "↻"), "▃ ▅ ▂ rpi · 🦀 ↻");
    }

    #[test]
    fn title_shares_the_lockup_opt_out() {
        // Disabling the crab must not leave a dangling separator in the title
        // either — the lockup and the tab title must never disagree.
        assert_eq!(window_title_from(false, ""), "▃ ▅ ▂ rpi");
        assert_eq!(window_title_from(false, "⟳"), "▃ ▅ ▂ rpi ⟳");
        assert!(!window_title_from(false, "").contains(CRAB));
    }

    #[test]
    fn title_has_no_theme_escapes_and_no_cjk_ambiguity() {
        let title = window_title_from(true, "⟳");
        assert_eq!(title, strip_ansi(&title), "OSC 2 titles must be plain text");
        // The mark is three block glyphs, each one cell wide, so a truncated
        // tab title cannot drift on a CJK system the way `π` did.
        assert_eq!(visible_width(MARK), 5, "mark width assumption changed");
    }

    #[test]
    fn emoji_width_is_the_one_thing_the_layout_depends_on() {
        // If a dependency bump changed this, the lockup would silently shift.
        assert_eq!(visible_width(CRAB), 2, "crab width assumption changed");
    }

    #[test]
    fn disabling_drops_the_crab_and_its_separator() {
        assert!(crab_enabled_from(None));
        for value in ["1", "true", "yes", "TRUE", "Yes"] {
            assert!(!crab_enabled_from(Some(value)), "{value} should disable");
        }
        for value in ["", "0", "false", "no", "off"] {
            assert!(crab_enabled_from(Some(value)), "{value} should not disable");
        }

        let rows = plain(false);
        assert_eq!(rows.len(), 5);
        assert!(!rows[4].contains(CRAB));
        // No dangling separator once the crab is gone.
        assert!(!rows[4].contains(SEPARATOR), "got {:?}", rows[4]);
        assert!(rows[4].ends_with("rpi"), "got {:?}", rows[4]);
    }
}
