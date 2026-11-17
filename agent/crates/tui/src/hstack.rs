//! HStack - Horizontal stack layout component.
//!
//! Arranges children horizontally from left to right.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::layout_node::{HStackNode, LayoutNode, LayoutNodeProvider, StackLayoutEntry};
use super::vstack::{StackAlign, StackEntry, StackEntryOptions, StackOptions};
use crate::ansi::{slice_by_column, visible_width};

/// HStack - Horizontal stack layout.
///
/// Children are arranged from left to right.
#[derive(Clone)]
pub struct HStack {
    children: Arc<Mutex<Vec<StackEntry>>>,
    options: StackOptions,
}

impl HStack {
    /// Create a new empty HStack.
    pub fn new() -> Self {
        Self {
            children: Arc::new(Mutex::new(Vec::new())),
            options: StackOptions::default(),
        }
    }

    /// Create a new HStack with options.
    pub fn with_options(options: StackOptions) -> Self {
        Self {
            children: Arc::new(Mutex::new(Vec::new())),
            options,
        }
    }

    /// Create an HStack from a vector of stack entries.
    pub fn from_entries(children: Vec<StackEntry>) -> Self {
        Self {
            children: Arc::new(Mutex::new(children)),
            options: StackOptions::default(),
        }
    }

    /// Add a child with default options.
    pub fn add_child(&self, component: Arc<dyn Component>) {
        if let Ok(mut children) = self.children.lock() {
            children.push(StackEntry::new(component));
        }
    }

    /// Add a child with options.
    pub fn add_child_with_options(
        &self,
        component: Arc<dyn Component>,
        options: StackEntryOptions,
    ) {
        if let Ok(mut children) = self.children.lock() {
            children.push(StackEntry::with_options(component, options));
        }
    }

    /// Remove a child.
    pub fn remove_child(&self, component: &Arc<dyn Component>) {
        if let Ok(mut children) = self.children.lock() {
            children.retain(|e| !Arc::ptr_eq(&e.component, component));
        }
    }

    /// Clear all children.
    pub fn clear(&self) {
        if let Ok(mut children) = self.children.lock() {
            children.clear();
        }
    }

    /// Get the gap between children.
    pub fn gap(&self) -> usize {
        self.options.gap
    }

    /// Get the alignment.
    pub fn align(&self) -> StackAlign {
        self.options.align
    }

    /// Get a clone of the children list.
    pub fn get_children(&self) -> Vec<StackEntry> {
        self.children.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Get layout entries for the layout system.
    pub fn get_layout_entries(&self) -> Vec<StackLayoutEntry> {
        self.children
            .lock()
            .map(|c| c.iter().map(|e| e.to_layout_entry()).collect())
            .unwrap_or_default()
    }
}

impl Default for HStack {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for HStack {
    fn render(&self, width: usize) -> Vec<String> {
        let children = self.children.lock().map(|c| c.clone()).unwrap_or_default();
        if children.is_empty() {
            return vec![String::new()];
        }

        // First pass: calculate widths and render children
        let mut child_data: Vec<(Vec<String>, usize)> = Vec::new();
        let mut total_basis = 0usize;
        let mut total_grow = 0usize;
        let mut grow_indices = Vec::new();

        for (i, entry) in children.iter().enumerate() {
            // Render child to get intrinsic height
            let lines = entry.component.render(width);
            let max_line_width = lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);

            let basis = entry.options.basis.unwrap_or(max_line_width);
            let min_size = entry.options.min_size;
            let max_size = entry.options.max_size.unwrap_or(width);

            let basis = basis.clamp(min_size, max_size);

            child_data.push((lines, basis));
            total_basis += basis;

            if entry.options.grow > 0 {
                total_grow += entry.options.grow;
                grow_indices.push(i);
            }
        }

        // Add gaps
        let total_gap = if children.len() > 1 {
            self.options.gap * (children.len() - 1)
        } else {
            0
        };
        total_basis += total_gap;

        // Distribute remaining space
        let available = width;
        if total_basis < available && total_grow > 0 {
            let remaining = available - total_basis;
            for &i in &grow_indices {
                let entry = &children[i];
                let share = (remaining * entry.options.grow) / total_grow;
                let max_size = entry.options.max_size.unwrap_or(width);
                let current = child_data[i].1;
                let new_width = (current + share).min(max_size);
                child_data[i].1 = new_width;
            }
        }

        // Find the max height
        let max_height = child_data
            .iter()
            .map(|(lines, _)| lines.len())
            .max()
            .unwrap_or(1);

        // Build output lines by compositing horizontally
        let mut result: Vec<String> = vec![String::new(); max_height];

        for (line_idx, result_line) in result.iter_mut().enumerate() {
            let mut current_col = 0;

            for (child_idx, (lines, child_width)) in child_data.iter().enumerate() {
                // Add gap
                if child_idx > 0 && self.options.gap > 0 {
                    result_line.push_str(&" ".repeat(self.options.gap));
                    current_col += self.options.gap;
                }

                // Get the line from this child (or empty string if too short)
                let child_line = if line_idx < lines.len() {
                    &lines[line_idx]
                } else {
                    ""
                };

                // Pad or truncate to allocated width
                let line_width = visible_width(child_line);
                if line_width < *child_width {
                    // Pad right based on alignment
                    match self.options.align {
                        StackAlign::Stretch | StackAlign::Start => {
                            result_line.push_str(child_line);
                            result_line.push_str(&" ".repeat(child_width - line_width));
                        }
                        StackAlign::Center => {
                            let left_pad = (child_width - line_width) / 2;
                            let right_pad = child_width - line_width - left_pad;
                            result_line.push_str(&" ".repeat(left_pad));
                            result_line.push_str(child_line);
                            result_line.push_str(&" ".repeat(right_pad));
                        }
                        StackAlign::End => {
                            result_line.push_str(&" ".repeat(child_width - line_width));
                            result_line.push_str(child_line);
                        }
                    }
                } else if line_width > *child_width {
                    // Truncate
                    let truncated = slice_by_column(child_line, 0, *child_width, true);
                    result_line.push_str(&truncated);
                } else {
                    result_line.push_str(child_line);
                }

                current_col += child_width;
            }

            // Ensure result doesn't exceed width
            if visible_width(result_line) > width {
                *result_line = slice_by_column(result_line, 0, width, true);
            }
        }

        result
    }

    fn invalidate(&self) {
        if let Ok(children) = self.children.lock() {
            for entry in children.iter() {
                entry.component.invalidate();
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl LayoutNodeProvider for HStack {
    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::HStack(HStackNode {
            entries: self.get_layout_entries(),
            gap: self.options.gap,
            align: self.options.align,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Text;

    #[test]
    fn test_hstack_horizontal_layout() {
        let hstack = HStack::new();
        hstack.add_child(Arc::new(Text::new("Hello", 0, 0)));
        hstack.add_child(Arc::new(Text::new("World", 0, 0)));

        let lines = hstack.render(20);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Hello"));
        assert!(lines[0].contains("World"));
    }
}
