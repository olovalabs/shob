//! Search bar component for transcript search.

use std::any::Any;
use std::sync::Mutex;

use crate::component::Component;
use crate::theme::theme;

/// Search bar component that displays search query and match count.
pub struct SearchBar {
    query: Mutex<String>,
    match_count: Mutex<usize>,
    current_match: Mutex<usize>,
    visible: Mutex<bool>,
}

impl SearchBar {
    pub fn new() -> Self {
        Self {
            query: Mutex::new(String::new()),
            match_count: Mutex::new(0),
            current_match: Mutex::new(0),
            visible: Mutex::new(false),
        }
    }

    pub fn set_query(&self, query: &str) {
        *self.query.lock().unwrap() = query.to_string();
    }

    pub fn set_match_info(&self, current: usize, total: usize) {
        *self.current_match.lock().unwrap() = current;
        *self.match_count.lock().unwrap() = total;
    }

    pub fn set_visible(&self, visible: bool) {
        *self.visible.lock().unwrap() = visible;
    }

    pub fn is_visible(&self) -> bool {
        *self.visible.lock().unwrap()
    }
}

impl Default for SearchBar {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for SearchBar {
    fn render(&self, width: usize) -> Vec<String> {
        if !self.is_visible() {
            return vec![];
        }

        let query = self.query.lock().unwrap().clone();
        let match_count = *self.match_count.lock().unwrap();
        let current_match = *self.current_match.lock().unwrap();

        let theme = theme();
        let colors = &theme.colors;

        // Build search bar text
        let search_icon = "🔍";
        let match_info = if match_count == 0 {
            if query.is_empty() {
                String::new()
            } else {
                "No matches".to_string()
            }
        } else {
            format!("{}/{}", current_match + 1, match_count)
        };

        let left = format!("{} Search: {}", search_icon, query);
        let right = match_info;

        // Calculate padding
        let left_len = left.chars().count();
        let right_len = right.chars().count();
        let padding = if width > left_len + right_len + 2 {
            width - left_len - right_len - 2
        } else {
            1
        };

        let line = format!(
            "{}{}{}{}",
            colors.background.fg(&left),
            " ".repeat(padding),
            colors.dim.fg(&right),
            " ".repeat(2)
        );

        vec![line]
    }

    fn invalidate(&self) {
        // No cached state to invalidate
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_bar_hidden() {
        let bar = SearchBar::new();
        assert_eq!(bar.render(80).len(), 0);
    }

    #[test]
    fn test_search_bar_visible() {
        let bar = SearchBar::new();
        bar.set_visible(true);
        bar.set_query("test");
        bar.set_match_info(0, 5);
        let lines = bar.render(80);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("test"));
        assert!(lines[0].contains("1/5"));
    }
}
