//! Truncated text component.
//!
//! Displays text that is truncated to fit within a given width.

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
use crate::utils::{truncate_to_width, visible_width};

/// Truncated text component.
pub struct TruncatedText {
    text: Mutex<String>,
    max_width: usize,
    ellipsis: String,
    style: Option<String>,
}

impl TruncatedText {
    /// Create a new truncated text.
    pub fn new(text: &str, max_width: usize) -> Self {
        Self {
            text: Mutex::new(text.to_string()),
            max_width,
            ellipsis: "...".to_string(),
            style: None,
        }
    }

    /// Create with custom ellipsis.
    pub fn with_ellipsis(text: &str, max_width: usize, ellipsis: &str) -> Self {
        Self {
            text: Mutex::new(text.to_string()),
            max_width,
            ellipsis: ellipsis.to_string(),
            style: None,
        }
    }

    /// Set text.
    pub fn set_text(&self, text: &str) {
        if let Ok(mut t) = self.text.lock() {
            *t = text.to_string();
        }
    }

    /// Get text.
    pub fn get_text(&self) -> String {
        self.text.lock().map(|t| t.clone()).unwrap_or_default()
    }

    /// Set max width.
    pub fn set_max_width(&mut self, width: usize) {
        self.max_width = width;
    }

    /// Set style (ANSI escape codes).
    pub fn set_style(&mut self, style: &str) {
        self.style = Some(style.to_string());
    }

    /// Check if text is truncated.
    pub fn is_truncated(&self) -> bool {
        let text = self.get_text();
        visible_width(&text) > self.max_width
    }
}

impl Component for TruncatedText {
    fn render(&self, width: usize) -> Vec<String> {
        let text = self.get_text();
        let effective_width = self.max_width.min(width);

        let truncated = truncate_to_width(&text, effective_width, &self.ellipsis);

        let styled = if let Some(ref style) = self.style {
            format!("{}{}\x1b[0m", style, truncated)
        } else {
            truncated
        };

        vec![styled]
    }

    fn invalidate(&self) {
        // No cached state
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncated_text() {
        let text = TruncatedText::new("Hello World", 5);
        let lines = text.render(10);
        assert!(lines[0].contains("H"));
    }

    #[test]
    fn test_is_truncated() {
        let text = TruncatedText::new("Hello World", 5);
        assert!(text.is_truncated());
    }

    #[test]
    fn test_not_truncated() {
        let text = TruncatedText::new("Hi", 10);
        assert!(!text.is_truncated());
    }
}
