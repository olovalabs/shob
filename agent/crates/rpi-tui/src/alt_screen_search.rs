//! Alt screen search functionality.
//!
//! Provides search functionality for the alternate screen TUI.

use std::sync::{Arc, Mutex};

use crate::utils::visible_width;

/// Search state.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    /// Current search query.
    pub query: String,
    /// Current match index.
    pub match_index: usize,
    /// All match positions (line, column).
    pub matches: Vec<SearchMatch>,
    /// Whether search is active.
    pub active: bool,
}

/// A search match.
#[derive(Debug, Clone, Copy)]
pub struct SearchMatch {
    /// Line index.
    pub line: usize,
    /// Start column.
    pub column: usize,
    /// Length of match.
    pub length: usize,
}

/// Alt screen search handler.
pub struct AltScreenSearch {
    state: Mutex<SearchState>,
    on_search: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    on_match: Mutex<Option<Arc<dyn Fn(usize, usize) + Send + Sync>>>,
    on_close: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl AltScreenSearch {
    /// Create a new search handler.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SearchState::default()),
            on_search: Mutex::new(None),
            on_match: Mutex::new(None),
            on_close: Mutex::new(None),
        }
    }

    /// Set search callback.
    pub fn on_search(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_search.lock() {
            *cb = Some(callback);
        }
    }

    /// Set match callback.
    pub fn on_match(&self, callback: Arc<dyn Fn(usize, usize) + Send + Sync>) {
        if let Ok(mut cb) = self.on_match.lock() {
            *cb = Some(callback);
        }
    }

    /// Set close callback.
    pub fn on_close(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut cb) = self.on_close.lock() {
            *cb = Some(callback);
        }
    }

    /// Check if search is active.
    pub fn is_active(&self) -> bool {
        self.state.lock().map(|s| s.active).unwrap_or(false)
    }

    /// Get current query.
    pub fn get_query(&self) -> String {
        self.state
            .lock()
            .map(|s| s.query.clone())
            .unwrap_or_default()
    }

    /// Get current match index.
    pub fn get_match_index(&self) -> usize {
        self.state.lock().map(|s| s.match_index).unwrap_or(0)
    }

    /// Get total match count.
    pub fn get_match_count(&self) -> usize {
        self.state.lock().map(|s| s.matches.len()).unwrap_or(0)
    }

    /// Activate search.
    pub fn activate(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.active = true;
            state.query.clear();
            state.matches.clear();
            state.match_index = 0;
        }
    }

    /// Deactivate search.
    pub fn deactivate(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.active = false;
        }

        if let Ok(cb) = self.on_close.lock() {
            if let Some(callback) = cb.as_ref() {
                callback();
            }
        }
    }

    /// Toggle search.
    pub fn toggle(&self) {
        if self.is_active() {
            self.deactivate();
        } else {
            self.activate();
        }
    }

    /// Update search query.
    pub fn set_query(&self, query: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.query = query.to_string();
            state.match_index = 0;
        }

        if let Ok(cb) = self.on_search.lock() {
            if let Some(callback) = cb.as_ref() {
                callback(query);
            }
        }
    }

    /// Add character to query.
    pub fn append_char(&self, c: char) {
        let new_query = format!("{}{}", self.get_query(), c);
        self.set_query(&new_query);
    }

    /// Remove last character from query.
    pub fn backspace(&self) {
        let mut query = self.get_query();
        if !query.is_empty() {
            query.pop();
            self.set_query(&query);
        }
    }

    /// Find matches in content.
    pub fn find_matches(&self, lines: &[String]) -> Vec<SearchMatch> {
        let query = self.get_query();
        if query.is_empty() {
            return Vec::new();
        }

        let query_lower = query.to_lowercase();
        let mut matches = Vec::new();

        for (line_idx, line) in lines.iter().enumerate() {
            let stripped = crate::utils::strip_ansi(line);
            let stripped_lower = stripped.to_lowercase();

            let mut start = 0;
            while let Some(pos) = stripped_lower[start..].find(&query_lower) {
                let abs_pos = start + pos;
                matches.push(SearchMatch {
                    line: line_idx,
                    column: abs_pos,
                    length: query.len(),
                });
                start = abs_pos + query.len();
            }
        }

        // Update state
        if let Ok(mut state) = self.state.lock() {
            state.matches = matches.clone();
        }

        matches
    }

    /// Go to next match.
    pub fn next_match(&self) -> Option<SearchMatch> {
        let matches = self.state.lock().ok()?.matches.clone();
        let count = matches.len();
        if count == 0 {
            return None;
        }

        // Get current match first
        let current_index = self.state.lock().ok()?.match_index;
        let match_result = matches[current_index];

        // Then move to next
        if let Ok(mut state) = self.state.lock() {
            state.match_index = (state.match_index + 1) % count;
        }

        if let Ok(cb) = self.on_match.lock() {
            if let Some(callback) = cb.as_ref() {
                callback(match_result.line, match_result.column);
            }
        }

        Some(match_result)
    }

    /// Go to previous match.
    pub fn previous_match(&self) -> Option<SearchMatch> {
        let matches = self.state.lock().ok()?.matches.clone();
        let count = matches.len();
        if count == 0 {
            return None;
        }

        let new_index = if let Ok(mut state) = self.state.lock() {
            state.match_index = if state.match_index == 0 {
                count - 1
            } else {
                state.match_index - 1
            };
            state.match_index
        } else {
            return None;
        };

        let match_result = matches[new_index];

        if let Ok(cb) = self.on_match.lock() {
            if let Some(callback) = cb.as_ref() {
                callback(match_result.line, match_result.column);
            }
        }

        Some(match_result)
    }

    /// Get current match.
    pub fn current_match(&self) -> Option<SearchMatch> {
        let state = self.state.lock().ok()?;
        if state.matches.is_empty() {
            return None;
        }
        state.matches.get(state.match_index).copied()
    }

    /// Render search bar.
    pub fn render(&self, width: usize) -> String {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        let match_info = if state.matches.is_empty() {
            "No matches".to_string()
        } else {
            format!("{}/{}", state.match_index + 1, state.matches.len())
        };

        let prompt = "Search: ";
        let prompt_len = visible_width(prompt);
        let info_len = visible_width(&match_info) + 3; // +3 for " | "

        let available = width.saturating_sub(prompt_len + info_len);

        let query_display = if state.query.len() > available {
            format!(
                "...{}",
                &state.query[state.query.len().saturating_sub(available)..]
            )
        } else {
            state.query.clone()
        };

        // Build the search bar with styling
        format!(
            "\x1b[7m{}{}\x1b[0m \x1b[90m{}\x1b[0m",
            prompt, query_display, match_info
        )
    }

    /// Highlight matches in a line.
    pub fn highlight_line(&self, line: &str) -> String {
        let query = self.get_query();
        if query.is_empty() {
            return line.to_string();
        }

        let query_lower = query.to_lowercase();
        let stripped = crate::utils::strip_ansi(line);
        let stripped_lower = stripped.to_lowercase();

        let mut result = String::new();
        let mut last_end = 0;
        let mut in_ansi = false;
        let mut _ansi_start = 0;
        let mut chars = line.char_indices().peekable();
        let mut visible_idx = 0;

        while let Some((byte_idx, c)) = chars.next() {
            // Track ANSI sequences
            if c == '\x1b' {
                in_ansi = true;
                _ansi_start = byte_idx;
                continue;
            }

            if in_ansi {
                if c == '\x07' || (('@'..='~').contains(&c) && c != '[') {
                    in_ansi = false;
                }
                continue;
            }

            // Check for match start
            if stripped_lower[visible_idx..].starts_with(&query_lower) {
                // Add any text before the match
                if byte_idx > last_end {
                    result.push_str(&line[last_end..byte_idx]);
                }

                // Add highlighted match
                let end_idx = byte_idx + query.len();
                result.push_str("\x1b[43m"); // Yellow background
                result.push_str(&line[byte_idx..end_idx.min(line.len())]);
                result.push_str("\x1b[0m");

                last_end = end_idx;
                visible_idx += query.len();

                // Skip ahead
                for _ in 1..query.len() {
                    if let Some((_, c)) = chars.next() {
                        if c != '\x1b' {
                            // Skip non-ANSI chars
                        }
                    }
                }
            } else {
                visible_idx += c.len_utf8();
            }
        }

        if last_end < line.len() {
            result.push_str(&line[last_end..]);
        }

        result
    }
}

impl Default for AltScreenSearch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_activate() {
        let search = AltScreenSearch::new();
        search.activate();
        assert!(search.is_active());
    }

    #[test]
    fn test_search_query() {
        let search = AltScreenSearch::new();
        search.set_query("test");
        assert_eq!(search.get_query(), "test");
    }

    #[test]
    fn test_find_matches() {
        let search = AltScreenSearch::new();
        search.set_query("hello");

        let lines = vec![
            "hello world".to_string(),
            "foo bar".to_string(),
            "hello again".to_string(),
        ];

        let matches = search.find_matches(&lines);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line, 0);
        assert_eq!(matches[1].line, 2);
    }

    #[test]
    fn test_next_match() {
        let search = AltScreenSearch::new();
        search.set_query("test");

        let lines = vec!["test one".to_string(), "test two".to_string()];

        search.find_matches(&lines);
        let m1 = search.next_match();
        assert_eq!(m1.unwrap().line, 0);

        let m2 = search.next_match();
        assert_eq!(m2.unwrap().line, 1);

        // Wraps around
        let m3 = search.next_match();
        assert_eq!(m3.unwrap().line, 0);
    }
}
