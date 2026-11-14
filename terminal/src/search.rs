//! Buffer search over the terminal grid, modelled on Zed's terminal search.
//!
//! Matches are collected from the whole scrollback with alacritty's
//! `RegexSearch`, highlighted by the renderer, and can be cycled with
//! next/previous, which scrolls the viewport to the active match.

use crate::terminal::TerminalState;
use alacritty_terminal::index::{Direction, Point};
use alacritty_terminal::term::search::{Match, RegexSearch};
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
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            regex: None,
            matches: Arc::from(Vec::<Match>::new()),
            current: None,
            use_regex: false,
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
        self.regex.is_some()
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
    }

    /// Set the search query and recompute the match list.
    ///
    /// Returns `true` when the query compiled successfully.
    pub fn set_query(&mut self, query: &str, use_regex: bool, state: &TerminalState) -> bool {
        self.query = query.to_string();
        self.use_regex = use_regex;
        self.current = None;
        self.matches = Arc::from(Vec::<Match>::new());

        if query.is_empty() {
            self.regex = None;
            return true;
        }

        let pattern = if use_regex {
            query.to_string()
        } else {
            escape_literal(query)
        };

        match RegexSearch::new(&pattern) {
            Ok(mut regex) => {
                self.matches = state.all_matches(&mut regex).into();
                self.regex = Some(regex);
                true
            }
            Err(_) => {
                self.regex = None;
                false
            }
        }
    }

    /// Recompute matches after the grid changed (new output, resize, …).
    pub fn refresh(&mut self, state: &TerminalState) {
        if self.regex.is_none() {
            return;
        }
        // Remember where the active match started so it can be re-selected
        // after the grid moved under us.
        let anchor = self.current_match().map(|m| *m.start());
        let Some(regex) = self.regex.as_mut() else {
            return;
        };
        self.matches = state.all_matches(regex).into();
        self.current =
            anchor.and_then(|point| self.matches.iter().position(|m| *m.start() == point));
    }

    /// Advance to the next/previous match and scroll it into view.
    ///
    /// Returns the selected match.
    pub fn advance(&mut self, direction: Direction, state: &TerminalState) -> Option<Match> {
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
