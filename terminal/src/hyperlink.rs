//! Link detection — URLs, `OSC 8` hyperlinks and path-like targets.
//!
//! This mirrors what Zed's terminal does: while the modifier key is held (or on
//! hover, depending on configuration) the cell under the mouse is resolved to a
//! link, the link is underlined, and clicking it hands the target to the
//! embedder.
//!
//! Three sources are consulted, in priority order:
//!
//! 1. `OSC 8` hyperlinks embedded by the application itself (`ls --hyperlink`,
//!    `gcc`, `cargo`, …). These are authoritative.
//! 2. The URL regex (same pattern Zed uses).
//! 3. A "path-like" regex, so `src/main.rs:42:9` in a compiler diagnostic
//!    becomes clickable with a row/column attached.

use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use std::ops::RangeInclusive;

/// Same pattern Zed uses in `crates/terminal/src/alacritty/hyperlinks.rs`.
pub const URL_REGEX: &str = r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`']+"#;

/// Path-ish run of characters, optionally suffixed with `:line[:column]` or
/// `(line,column)` so compiler/linter output is clickable.
pub const PATH_REGEX: &str =
    r#"[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"'`\s|,;!?*()\[\]{}]+(:\d+(:\d+)?)?"#;

/// Whitespace separated word, used for the "select word under cursor" fallback.
pub const WORD_REGEX: &str = r#"[^\u{0000}-\u{001F}\u{007F}-\u{009F}\s]+"#;

/// Maximum number of lines searched above/below the viewport when resolving a
/// link, so a huge scrollback cannot stall a frame.
pub const MAX_SEARCH_LINES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyperlinkKind {
    /// An `OSC 8` hyperlink emitted by the application.
    Osc8,
    /// Matched the URL regex.
    Url,
    /// Matched the path regex.
    Path,
    /// Matched the word regex.
    Word,
}

/// A resolved link under the mouse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlinkMatch {
    /// Raw text of the match (for `Osc8` this is the URI, not the label).
    pub text: String,
    /// Grid range the match covers, used to underline it.
    pub range: RangeInclusive<Point>,
    pub kind: HyperlinkKind,
}

impl HyperlinkMatch {
    pub fn contains(&self, point: Point) -> bool {
        self.range.contains(&point)
    }

    /// Split a `path:line:column` style target into its parts.
    ///
    /// Returns `(path, line, column)` with 1-based line/column numbers when
    /// present. Non path-like matches are returned unchanged.
    pub fn path_with_position(&self) -> (String, Option<u32>, Option<u32>) {
        parse_path_with_position(&self.text)
    }

    /// `true` when this target should be handed to a browser rather than the
    /// editor.
    pub fn is_url(&self) -> bool {
        match self.kind {
            HyperlinkKind::Url => true,
            HyperlinkKind::Osc8 => true,
            _ => false,
        }
    }
}

/// Parse `path`, `path:12` and `path:12:34` (and the `path(12,34)` variant used
/// by MSVC and some JS tools).
pub fn parse_path_with_position(text: &str) -> (String, Option<u32>, Option<u32>) {
    // `path(line,column)`
    if let Some(open) = text.rfind('(') {
        if text.ends_with(')') {
            let inner = &text[open + 1..text.len() - 1];
            let mut parts = inner.split(',');
            let line = parts.next().and_then(|p| p.trim().parse::<u32>().ok());
            let column = parts.next().and_then(|p| p.trim().parse::<u32>().ok());
            if line.is_some() && parts.next().is_none() {
                return (text[..open].to_string(), line, column);
            }
        }
    }

    // `path:line:column`
    let mut parts = text.rsplitn(3, ':');
    let last = parts.next();
    let middle = parts.next();
    let head = parts.next();

    match (head, middle, last) {
        (Some(head), Some(middle), Some(last)) => {
            match (middle.parse::<u32>(), last.parse::<u32>()) {
                (Ok(line), Ok(column)) if !head.is_empty() => {
                    return (head.to_string(), Some(line), Some(column));
                }
                _ => {}
            }
            // Maybe it is only `path:line` where `path` itself contains a colon.
            if let Ok(line) = last.parse::<u32>() {
                return (format!("{head}:{middle}"), Some(line), None);
            }
        }
        (None, Some(head), Some(last)) => {
            if let Ok(line) = last.parse::<u32>() {
                if !head.is_empty() {
                    return (head.to_string(), Some(line), None);
                }
            }
        }
        _ => {}
    }

    (text.to_string(), None, None)
}

/// Lazily compiled regexes, kept alive across frames because
/// `RegexSearch::new` is expensive.
pub struct RegexSearches {
    pub url: Option<RegexSearch>,
    pub path: Option<RegexSearch>,
    pub word: Option<RegexSearch>,
}

impl Default for RegexSearches {
    fn default() -> Self {
        Self::new()
    }
}

impl RegexSearches {
    pub fn new() -> Self {
        Self {
            url: RegexSearch::new(URL_REGEX).ok(),
            path: RegexSearch::new(PATH_REGEX).ok(),
            word: RegexSearch::new(WORD_REGEX).ok(),
        }
    }
}

/// All matches of `regex` within (roughly) the visible region.
pub fn visible_regex_matches<T>(term: &Term<T>, regex: &mut RegexSearch) -> Vec<Match> {
    use alacritty_terminal::grid::Dimensions as _;

    let display_offset = term.grid().display_offset();
    let viewport_start = -(display_offset as i32);
    let viewport_end = viewport_start + term.bottommost_line().0;

    let mut start = term.line_search_left(Point::new(Line(viewport_start), Column(0)));
    let mut end = term.line_search_right(Point::new(Line(viewport_end), Column(0)));

    start.line = Line(
        start
            .line
            .0
            .max(viewport_start.saturating_sub(MAX_SEARCH_LINES as i32)),
    );
    end.line = Line(
        end.line
            .0
            .min(viewport_end.saturating_add(MAX_SEARCH_LINES as i32)),
    );

    RegexIter::new(start, end, Direction::Right, term, regex).collect()
}

/// The regex match containing `point`, if any.
///
/// Only the hovered (wrapped) line is searched: this runs on every mouse move,
/// and scanning the whole viewport for every motion event stalls frames.
/// A link under the cursor always covers the cursor's own line, so searching
/// just that line finds exactly the same match.
pub fn regex_match_at<T>(term: &Term<T>, point: Point, regex: &mut RegexSearch) -> Option<Match> {
    let start = term.line_search_left(point);
    let end = term.line_search_right(point);
    RegexIter::new(start, end, Direction::Right, term, regex).find(|m| m.contains(&point))
}

/// Text covered by a grid range.
pub fn match_text<T>(term: &Term<T>, m: &Match) -> String {
    term.bounds_to_string(*m.start(), *m.end())
}

/// Expand an `OSC 8` hyperlink run around `point`.
fn osc8_match_at<T>(term: &Term<T>, point: Point) -> Option<HyperlinkMatch> {
    let grid = term.grid();
    let hyperlink = grid[point].hyperlink()?;
    let id = hyperlink.id().to_string();
    let uri = hyperlink.uri().to_string();

    let same_link = |p: Point| -> bool {
        grid[p]
            .hyperlink()
            .map(|h| h.id() == id && h.uri() == uri)
            .unwrap_or(false)
    };

    let mut start = point;
    loop {
        let candidate = start.sub(term, Boundary::Grid, 1);
        if candidate == start || !same_link(candidate) {
            break;
        }
        start = candidate;
    }

    let mut end = point;
    loop {
        let candidate = end.add(term, Boundary::Grid, 1);
        if candidate == end || !same_link(candidate) {
            break;
        }
        end = candidate;
    }

    Some(HyperlinkMatch {
        text: uri,
        range: start..=end,
        kind: HyperlinkKind::Osc8,
    })
}

/// Resolve the link under `point`, if there is one.
///
/// `include_paths` controls whether path-like targets are considered; URLs and
/// `OSC 8` links are always considered.
pub fn find_at<T>(
    term: &Term<T>,
    point: Point,
    searches: &mut RegexSearches,
    include_paths: bool,
) -> Option<HyperlinkMatch> {
    if let Some(osc8) = osc8_match_at(term, point) {
        return Some(osc8);
    }

    if let Some(regex) = searches.url.as_mut() {
        if let Some(m) = regex_match_at(term, point, regex) {
            return Some(HyperlinkMatch {
                text: match_text(term, &m),
                range: m,
                kind: HyperlinkKind::Url,
            });
        }
    }

    if include_paths {
        if let Some(regex) = searches.path.as_mut() {
            if let Some(m) = regex_match_at(term, point, regex) {
                let text = match_text(term, &m);
                // Reject bare words: a path target should at least look like a
                // path or carry a `:line` suffix.
                let (path, line, _) = parse_path_with_position(&text);
                let looks_like_path = line.is_some()
                    || path.contains('/')
                    || path.contains('\\')
                    || (path.contains('.') && !path.ends_with('.'));
                if looks_like_path {
                    return Some(HyperlinkMatch {
                        text,
                        range: m,
                        kind: HyperlinkKind::Path,
                    });
                }
            }
        }
    }

    None
}

/// The whitespace separated word under `point`, used by double-click when the
/// semantic selection is not what we want.
pub fn word_at<T>(
    term: &Term<T>,
    point: Point,
    searches: &mut RegexSearches,
) -> Option<HyperlinkMatch> {
    let regex = searches.word.as_mut()?;
    let m = regex_match_at(term, point, regex)?;
    Some(HyperlinkMatch {
        text: match_text(term, &m),
        range: m,
        kind: HyperlinkKind::Word,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regexes_compile() {
        assert!(RegexSearch::new(URL_REGEX).is_ok());
        assert!(RegexSearch::new(PATH_REGEX).is_ok());
        assert!(RegexSearch::new(WORD_REGEX).is_ok());
    }

    #[test]
    fn test_parse_path_with_position() {
        assert_eq!(
            parse_path_with_position("src/main.rs:42:9"),
            ("src/main.rs".to_string(), Some(42), Some(9))
        );
        assert_eq!(
            parse_path_with_position("src/main.rs:42"),
            ("src/main.rs".to_string(), Some(42), None)
        );
        assert_eq!(
            parse_path_with_position("src/main.rs"),
            ("src/main.rs".to_string(), None, None)
        );
        assert_eq!(
            parse_path_with_position("src\\main.cpp(42,9)"),
            ("src\\main.cpp".to_string(), Some(42), Some(9))
        );
    }

    #[test]
    fn test_windows_drive_path_is_not_mangled() {
        // `C:\x` must not be read as "path C with line x".
        let (path, line, column) = parse_path_with_position("C:\\src\\main.rs");
        assert_eq!(path, "C:\\src\\main.rs");
        assert_eq!(line, None);
        assert_eq!(column, None);
    }

    #[test]
    fn test_url_match_is_marked_as_url() {
        let m = HyperlinkMatch {
            text: "https://zed.dev".into(),
            range: Point::new(Line(0), Column(0))..=Point::new(Line(0), Column(14)),
            kind: HyperlinkKind::Url,
        };
        assert!(m.is_url());
        assert!(m.contains(Point::new(Line(0), Column(3))));
        assert!(!m.contains(Point::new(Line(1), Column(3))));
    }
}
