//! Buffer search over the terminal grid, modelled on Zed's terminal search.
//!
//! Matches are collected from the whole scrollback with alacritty's
//! `RegexSearch`, highlighted by the renderer, and can be cycled with
//! next/previous, which scrolls the viewport to the active match.

use crate::terminal::TerminalState;
use crate::terminal::{MAX_SEARCH_MATCHES, TermDimensions};
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{Config, Term};
use std::sync::Arc;

/// Escape a literal string so it can be used as a regex.
pub fn escape_literal(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for ch in query.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

pub(crate) struct SearchSnapshot {
    grid: Grid<Cell>,
    top: i32,
}

impl SearchSnapshot {
    pub(crate) fn new(columns: usize, lines: usize, top: i32) -> Self {
        Self {
            grid: Grid::new(lines, columns, 0),
            top,
        }
    }

    pub(crate) fn copy_cells<T>(&mut self, term: &Term<T>, start: usize, count: usize) {
        let columns = self.grid.columns();
        let mut copied = 0;
        while copied < count {
            let index = start + copied;
            let row = index / columns;
            let col = index % columns;
            let to = (col + count - copied).min(columns);
            let source = &term.grid()[Line(row as i32 + self.top)];
            let dest = &mut self.grid[Line(row as i32)];
            for col in col..to {
                let source = &source[Column(col)];
                let dest = &mut dest[Column(col)];
                dest.c = source.c;
                dest.flags = source.flags;
            }
            copied += to - col;
        }
    }

    pub(crate) fn matches(self, regex: &mut RegexSearch) -> Arc<[Match]> {
        let mut term = Term::new(Config::default(), &TermDimensions::new(2, 1), VoidListener);
        *term.grid_mut() = self.grid;
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(term.bottommost_line(), term.last_column());
        let shift = |point: &Point| Point::new(point.line + self.top, point.column);
        RegexIter::new(start, end, Direction::Right, &term, regex)
            .take(MAX_SEARCH_MATCHES)
            .map(|range| shift(range.start())..=shift(range.end()))
            .collect()
    }
}

pub struct SearchState {
    query: String,
    regex: Option<RegexSearch>,
    /// Immutable snapshot so render closures can share the current list without
    /// cloning every match on each repaint. A new snapshot is built only when
    /// the query or terminal contents change.
    matches: Arc<[Match]>,
    current: Option<usize>,
    /// When `false` the query is treated as a literal string.
    use_regex: bool,
    caches_released: bool,
    scanning: bool,
    released_anchor: Option<Point>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            regex: None,
            matches: Arc::from(Vec::<Match>::new()),
            current: None,
            use_regex: false,
            caches_released: false,
            scanning: false,
            released_anchor: None,
        }
    }
}

impl SearchState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn is_active(&self) -> bool {
        self.regex.is_some() || self.caches_released || self.scanning
    }

    pub fn use_regex(&self) -> bool {
        self.use_regex
    }

    pub fn matches(&self) -> &[Match] {
        &self.matches
    }

    /// Cheaply clone the immutable search snapshot for a deferred paint closure.
    pub fn matches_arc(&self) -> Arc<[Match]> {
        Arc::clone(&self.matches)
    }

    /// Index of the currently selected match, if any.
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn current_match(&self) -> Option<&Match> {
        self.current.and_then(|i| self.matches.get(i))
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.regex = None;
        self.matches = Arc::from(Vec::<Match>::new());
        self.current = None;
        self.caches_released = false;
        self.scanning = false;
        self.released_anchor = None;
    }

    pub(crate) fn release_idle_resources(&mut self) {
        self.released_anchor = self
            .current_match()
            .map(|m| *m.start())
            .or(self.released_anchor);
        self.caches_released = self.is_active();
        self.scanning = false;
        self.regex = None;
        self.matches = Arc::from(Vec::<Match>::new());
        self.current = None;
    }

    /// Set the search query and recompute the match list.
    ///
    /// Returns `true` when the query compiled successfully.
    pub fn set_query(&mut self, query: &str, use_regex: bool, state: &TerminalState) -> bool {
        let ok = self.prepare_query(query, use_regex);
        if let Some(regex) = self.regex.as_mut() {
            self.matches = state.all_matches(regex).into();
        }
        ok
    }

    pub(crate) fn prepare_query(&mut self, query: &str, use_regex: bool) -> bool {
        self.scanning = false;
        self.query = query.to_string();
        self.use_regex = use_regex;
        self.current = None;
        self.matches = Arc::from(Vec::<Match>::new());
        self.caches_released = false;
        self.released_anchor = None;

        if query.is_empty() {
            self.regex = None;
            return true;
        }

        let pattern = if use_regex {
            query.to_string()
        } else {
            escape_literal(query)
        };

        self.regex = RegexSearch::new(&pattern).ok();
        self.regex.is_some()
    }

    /// Recompute matches after the grid changed (new output, resize, …).
    pub fn refresh(&mut self, state: &TerminalState) {
        if !self.is_active() {
            return;
        }
        self.restore_regex_cache();
        let Some(regex) = self.regex.as_mut() else {
            return;
        };
        let matches = state.all_matches(regex).into();
        self.set_matches(matches);
    }

    pub(crate) fn invalidate_matches(&mut self) {
        self.released_anchor = self
            .current_match()
            .map(|m| *m.start())
            .or(self.released_anchor);
        self.current = None;
        if !self.matches.is_empty() {
            self.matches = Arc::from([]);
        }
    }

    pub(crate) fn take_regex(&mut self) -> Option<RegexSearch> {
        self.restore_regex_cache();
        let regex = self.regex.take()?;
        self.scanning = true;
        Some(regex)
    }

    pub(crate) fn finish_scan(&mut self, regex: RegexSearch) {
        self.scanning = false;
        self.regex = Some(regex);
    }

    fn restore_regex_cache(&mut self) {
        if self.caches_released {
            let pattern = if self.use_regex {
                self.query.clone()
            } else {
                escape_literal(&self.query)
            };
            self.regex = RegexSearch::new(&pattern).ok();
            self.caches_released = false;
        }
    }

    pub(crate) fn set_matches(&mut self, matches: Arc<[Match]>) {
        let anchor = self
            .current_match()
            .map(|m| *m.start())
            .or(self.released_anchor.take());
        self.matches = matches;
        self.current =
            anchor.and_then(|point| self.matches.iter().position(|m| *m.start() == point));
    }

    /// Advance to the next/previous match and scroll it into view.
    ///
    /// Returns the selected match.
    pub fn advance(&mut self, direction: Direction, state: &TerminalState) -> Option<Match> {
        if self.caches_released {
            self.refresh(state);
        }
        if self.matches.is_empty() {
            return None;
        }

        let next = match (self.current, direction) {
            (None, Direction::Right) => 0,
            (None, Direction::Left) => self.matches.len() - 1,
            (Some(current), Direction::Right) => (current + 1) % self.matches.len(),
            (Some(current), Direction::Left) => {
                (current + self.matches.len() - 1) % self.matches.len()
            }
        };

        self.current = Some(next);
        let selected = self.matches[next].clone();
        state.scroll_to_point(*selected.start());
        Some(selected)
    }

    /// Select the first match at or after `origin`.
    pub fn select_first_after(&mut self, origin: Point, state: &TerminalState) -> Option<Match> {
        if self.caches_released {
            self.refresh(state);
        }
        let index = self.matches.iter().position(|m| *m.start() >= origin).or(
            if self.matches.is_empty() {
                None
            } else {
                Some(0)
            },
        )?;
        self.current = Some(index);
        let selected = self.matches[index].clone();
        state.scroll_to_point(*selected.start());
        Some(selected)
    }

    /// `true` when `point` is inside any match.
    pub fn contains(&self, point: Point) -> bool {
        self.matches.iter().any(|m| m.contains(&point))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::GpuiEventProxy;
    use std::sync::mpsc::channel;

    fn state_with(content: &[u8]) -> TerminalState {
        let (tx, _rx) = channel();
        let mut state = TerminalState::new(40, 8, GpuiEventProxy::new(tx));
        state.process_bytes(content);
        state
    }

    #[test]
    fn search_snapshot_keeps_wrapped_matches_and_scrollback_coordinates() {
        let state = state_with(
            b"012345678901234567890123456789012345678wrap\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\nhit",
        );
        let mut regex = RegexSearch::new("wrap|hit").unwrap();
        let expected = state.all_matches(&mut regex);
        assert_eq!(expected.len(), 2);
        assert!(expected[0].start().line.0 < 0);
        assert_ne!(expected[0].start().line, expected[0].end().line);
        let snapshot = state.with_term(|term| {
            let columns = term.columns();
            let lines = term.total_lines();
            let mut snapshot = SearchSnapshot::new(columns, lines, term.topmost_line().0);
            let split = columns + 3;
            snapshot.copy_cells(term, 0, split);
            snapshot.copy_cells(term, split, columns * lines - split);
            snapshot
        });
        assert_eq!(&*snapshot.matches(&mut regex), expected);
    }

    #[test]
    fn releasing_caches_preserves_query_and_selected_match() {
        let state = state_with(b"one1\r\ntwo2\r\nthree3\r\n");
        let mut search = SearchState::new();
        assert!(search.set_query(r"\d", true, &state));
        search.advance(Direction::Right, &state);
        let selected = search.advance(Direction::Right, &state).unwrap();
        let display_offset = state.with_term(|term| term.grid().display_offset());

        search.release_idle_resources();
        search.release_idle_resources();
        assert_eq!(search.query(), r"\d");
        assert!(search.use_regex());
        assert!(search.is_active());
        assert!(search.regex.is_none());
        assert!(search.matches().is_empty());

        search.refresh(&state);
        assert_eq!(search.current_match(), Some(&selected));
        assert_eq!(search.match_count(), 3);
        assert_eq!(
            state.with_term(|term| term.grid().display_offset()),
            display_offset
        );
        search.release_idle_resources();
        assert_eq!(
            search
                .advance(Direction::Left, &state)
                .unwrap()
                .start()
                .line,
            alacritty_terminal::index::Line(0)
        );
    }

    #[test]
    fn test_escape_literal() {
        assert_eq!(escape_literal("a.b*c"), "a\\.b\\*c");
        assert_eq!(escape_literal("plain"), "plain");
    }

    #[test]
    fn test_literal_search() {
        let state = state_with(b"alpha\r\nbeta\r\nalpha\r\n");
        let mut search = SearchState::new();

        assert!(search.set_query("alpha", false, &state));
        assert_eq!(search.match_count(), 2);
        assert!(search.is_active());
    }

    #[test]
    fn test_regex_search_and_cycling() {
        let state = state_with(b"one1\r\ntwo2\r\nthree3\r\n");
        let mut search = SearchState::new();

        assert!(search.set_query(r"\d", true, &state));
        assert_eq!(search.match_count(), 3);

        assert!(search.advance(Direction::Right, &state).is_some());
        assert_eq!(search.current_index(), Some(0));
        search.advance(Direction::Right, &state);
        assert_eq!(search.current_index(), Some(1));
        search.advance(Direction::Left, &state);
        assert_eq!(search.current_index(), Some(0));
        // Wraps around backwards.
        search.advance(Direction::Left, &state);
        assert_eq!(search.current_index(), Some(2));
    }

    #[test]
    fn test_invalid_regex_is_reported() {
        let state = state_with(b"hello\r\n");
        let mut search = SearchState::new();
        assert!(!search.set_query("(unclosed", true, &state));
        assert!(!search.is_active());
    }

    #[test]
    fn test_clear() {
        let state = state_with(b"hello\r\n");
        let mut search = SearchState::new();
        search.set_query("hello", false, &state);
        assert!(search.is_active());
        search.clear();
        assert!(!search.is_active());
        assert_eq!(search.match_count(), 0);
    }

    /// Measurement, not a correctness check: how long does one full-scrollback
    /// rescan take while output streams?
    ///
    /// `cargo test --release -p terminal -- --ignored --nocapture search_refresh`
    #[test]
    #[ignore = "measurement; run with --release --ignored --nocapture"]
    fn search_refresh_cost_over_full_scrollback() {
        let (tx, _rx) = channel();
        let mut state = TerminalState::new(120, 40, GpuiEventProxy::new(tx));
        // Default config keeps 10_000 lines of scrollback.
        for i in 0..20_000 {
            state.process_bytes(
                format!("line {i}: the quick brown fox jumps over the lazy dog\r\n").as_bytes(),
            );
        }
        let mut search = SearchState::new();
        assert!(search.set_query("quick brown", false, &state));

        let rounds = 50;
        let mut total = std::time::Duration::ZERO;
        let mut worst = std::time::Duration::ZERO;
        for _ in 0..rounds {
            state.process_bytes(b"one more line of streamed output\r\n");
            let start = std::time::Instant::now();
            search.refresh(&state);
            let elapsed = start.elapsed();
            total += elapsed;
            worst = worst.max(elapsed);
        }
        eprintln!(
            "search refresh over {} history lines x {} cols: avg={:.2}ms max={:.2}ms matches={}",
            state.history_size(),
            state.cols(),
            total.as_secs_f64() * 1000.0 / rounds as f64,
            worst.as_secs_f64() * 1000.0,
            search.match_count()
        );
    }
}
