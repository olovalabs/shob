//! Box component - a container that applies padding and background to children.
//!
//! Similar to a div with padding and background in CSS.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use crate::utils::{apply_background_to_line, visible_width};

/// Render cache for Box component.
#[derive(Debug, Clone)]
struct RenderCache {
    child_lines: Vec<String>,
    width: usize,
    bg_sample: Option<String>,
    lines: Vec<String>,
}

/// Box component - a container with padding and optional background.
pub struct Box {
    children: Mutex<Vec<Arc<dyn Component>>>,
    padding_x: usize,
    padding_y: usize,
    bg_fn: Mutex<Option<Arc<dyn Fn(&str) -> String + Send + Sync>>>,
    cache: Mutex<Option<RenderCache>>,
}

impl Box {
    /// Create a new box with specified padding.
    pub fn new(padding_x: usize, padding_y: usize) -> Self {
        Self {
            children: Mutex::new(Vec::new()),
            padding_x,
            padding_y,
            bg_fn: Mutex::new(None),
            cache: Mutex::new(None),
        }
    }

    /// Create a box with default padding (1, 1).
    pub fn simple() -> Self {
        Self::new(1, 1)
    }

    /// Create a box with no padding.
    pub fn no_padding() -> Self {
        Self::new(0, 0)
    }

    /// Add a child component.
    pub fn add_child(&self, component: Arc<dyn Component>) {
        if let Ok(mut children) = self.children.lock() {
            children.push(component);
        }
        self.invalidate_cache();
    }

    /// Remove a child component.
    pub fn remove_child(&self, component: &Arc<dyn Component>) {
        if let Ok(mut children) = self.children.lock() {
            children.retain(|c| !Arc::ptr_eq(c, component));
        }
        self.invalidate_cache();
    }

    /// Clear all children.
    pub fn clear(&self) {
        if let Ok(mut children) = self.children.lock() {
            children.clear();
        }
        self.invalidate_cache();
    }

    /// Set the background function.
    pub fn set_bg_fn(&self, f: Option<Arc<dyn Fn(&str) -> String + Send + Sync>>) {
        if let Ok(mut bg_fn) = self.bg_fn.lock() {
            *bg_fn = f;
        }
        // Don't invalidate - we'll detect bg_fn changes by sampling output
    }

    /// Invalidate the render cache.
    fn invalidate_cache(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
    }

    /// Check if the cache is valid.
    fn is_cache_valid(
        &self,
        width: usize,
        child_lines: &[String],
        bg_sample: Option<&str>,
    ) -> bool {
        if let Ok(cache) = self.cache.lock() {
            if let Some(ref cache) = *cache {
                return cache.width == width
                    && cache.bg_sample.as_deref() == bg_sample
                    && cache.child_lines.len() == child_lines.len()
                    && cache
                        .child_lines
                        .iter()
                        .zip(child_lines.iter())
                        .all(|(a, b)| a == b);
            }
        }
        false
    }

    /// Apply background to a line.
    fn apply_bg(&self, line: &str, width: usize) -> String {
        if let Ok(bg_fn) = self.bg_fn.lock() {
            if let Some(ref f) = *bg_fn {
                return apply_background_to_line(line, width, |s| f(s));
            }
        }
        // Pad with spaces
        let vis_len = visible_width(line);
        let pad = " ".repeat(width.saturating_sub(vis_len));
        format!("{}{}", line, pad)
    }
}

impl Component for Box {
    fn render(&self, width: usize) -> Vec<String> {
        let children = if let Ok(children) = self.children.lock() {
            children.clone()
        } else {
            return Vec::new();
        };

        if children.is_empty() {
            return Vec::new();
        }

        let content_width = width.saturating_sub(self.padding_x * 2);
        let left_pad = " ".repeat(self.padding_x);

        // Render all children
        let mut child_lines: Vec<String> = Vec::new();
        for child in &children {
            let lines = child.render(content_width);
            for line in lines {
                child_lines.push(format!("{}{}", left_pad, line));
            }
        }

        if child_lines.is_empty() {
            return Vec::new();
        }

        // Sample bg_fn output for cache validation
        let bg_sample = if let Ok(bg_fn) = self.bg_fn.lock() {
            bg_fn.as_ref().map(|f| f("test"))
        } else {
            None
        };

        // Check cache validity
        if self.is_cache_valid(width, &child_lines, bg_sample.as_deref()) {
            if let Ok(cache) = self.cache.lock() {
                if let Some(ref cache) = *cache {
                    return cache.lines.clone();
                }
            }
        }

        // Apply background and padding
        let mut result: Vec<String> = Vec::new();

        // Top padding
        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }

        // Content
        for line in &child_lines {
            result.push(self.apply_bg(line, width));
        }

        // Bottom padding
        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }

        // Update cache
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(RenderCache {
                child_lines,
                width,
                bg_sample,
                lines: result.clone(),
            });
        }

        result
    }

    fn invalidate(&self) {
        self.invalidate_cache();
        if let Ok(children) = self.children.lock() {
            for child in &*children {
                child.invalidate();
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Text;

    #[test]
    fn test_box_padding() {
        let box_component = Box::new(2, 1);
        let text = Arc::new(Text::new("Hello", 1, 0));
        box_component.add_child(text);

        let lines = box_component.render(10);
        assert!(!lines.is_empty());

        // Should have top padding, content, and bottom padding
        assert!(lines.len() >= 3);
    }

    #[test]
    fn test_box_empty() {
        let box_component = Box::new(1, 1);
        let lines = box_component.render(10);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_box_clear() {
        let box_component = Box::new(1, 1);
        let text = Arc::new(Text::new("Hello", 1, 0));
        box_component.add_child(text);
        box_component.clear();

        let lines = box_component.render(10);
        assert!(lines.is_empty());
    }
}
