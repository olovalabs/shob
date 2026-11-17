//! Component trait and base types for TUI.
//!
//! Mirrors the TypeScript `Component` interface from `packages/tui/src/tui.ts`.

use std::any::Any;
use std::sync::Arc;

/// Component interface - all components must implement this.
///
/// Components are the building blocks of the TUI. Each component can render
/// itself to a list of strings (one per line) for a given viewport width.
pub trait Component: Send + Sync {
    /// Render the component to lines for the given viewport width.
    ///
    /// # Arguments
    /// * `width` - Current viewport width in columns
    ///
    /// # Returns
    /// Array of strings, each representing a line
    fn render(&self, width: usize) -> Vec<String>;

    /// Render shared row blocks for a scrollable document. Components with
    /// cached output can override this to avoid copying off-screen history.
    /// The default deliberately renders afresh: arbitrary mutable components
    /// must never be assumed immutable merely because their identity is stable.
    fn render_document(&self, width: usize) -> RenderedDocument {
        RenderedDocument::from_lines(self.render(width))
    }

    /// Optional handler for keyboard input when component has focus.
    ///
    /// # Arguments
    /// * `data` - Raw input data (key sequence)
    ///
    /// # Returns
    /// true if the input was consumed, false otherwise
    fn handle_input(&self, _data: &str) -> bool {
        false
    }

    /// If true, component receives key release events (Kitty protocol).
    /// Default is false - release events are filtered out.
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Invalidate any cached rendering state.
    /// Called when theme changes or when component needs to re-render from scratch.
    fn invalidate(&self);

    /// Downcast to Any for runtime type checking.
    /// This enables the layout system to check for LayoutNodeProvider.
    fn as_any(&self) -> &dyn Any;
}

/// A rendered document assembled from immutable, shared row blocks.
///
/// Composing documents copies only block handles, not historical strings.
/// Cumulative block ends permit viewport extraction without walking every row.
#[derive(Clone, Default)]
pub struct RenderedDocument {
    blocks: Vec<Arc<Vec<String>>>,
    ends: Vec<usize>,
}

impl RenderedDocument {
    /// Own a freshly rendered block.
    pub fn from_lines(lines: Vec<String>) -> Self {
        Self::from_shared(Arc::new(lines))
    }

    /// Retain cached rows without copying their contents.
    pub fn from_shared(lines: Arc<Vec<String>>) -> Self {
        if lines.is_empty() {
            return Self::default();
        }
        Self {
            ends: vec![lines.len()],
            blocks: vec![lines],
        }
    }

    /// Append another document, retaining its shared row allocations.
    pub fn append(&mut self, other: Self) {
        let offset = self.len();
        self.ends
            .extend(other.ends.into_iter().map(|end| offset + end));
        self.blocks.extend(other.blocks);
    }

    /// Total rendered row count.
    pub fn len(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }

    /// Whether the document contains no rows.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Borrow a single row, e.g. to inspect a row that scrolled out of view.
    pub fn line(&self, index: usize) -> Option<&str> {
        let block_index = self.ends.partition_point(|end| *end <= index);
        let block = self.blocks.get(block_index)?;
        let start = if block_index == 0 {
            0
        } else {
            self.ends[block_index - 1]
        };
        block.get(index - start).map(String::as_str)
    }

    /// Copy only the requested rows, typically the visible viewport.
    pub fn lines(&self, start: usize, height: usize) -> Vec<String> {
        let mut index = self.ends.partition_point(|end| *end <= start);
        let mut offset = start.saturating_sub(if index == 0 { 0 } else { self.ends[index - 1] });
        let mut visible = Vec::with_capacity(height.min(self.len().saturating_sub(start)));
        while index < self.blocks.len() && visible.len() < height {
            let block = &self.blocks[index];
            visible.extend(
                block
                    .iter()
                    .skip(offset)
                    .take(height - visible.len())
                    .cloned(),
            );
            index += 1;
            offset = 0;
        }
        visible
    }
}

/// Interface for components that can receive focus and display a hardware cursor.
///
/// When focused, the component should emit `CURSOR_MARKER` at the cursor position
/// in its render output. TUI will find this marker and position the hardware
/// cursor there for proper IME candidate window positioning.
pub trait Focusable: Component {
    /// Set by TUI when focus changes. Component should emit CURSOR_MARKER when true.
    fn set_focused(&self, focused: bool);

    /// Check if the component is currently focused.
    fn is_focused(&self) -> bool;
}

/// Type alias for a reference-counted component.
pub type ComponentRef = Arc<dyn Component>;

/// Type alias for a reference-counted focusable component.
pub type FocusableRef = Arc<dyn Focusable>;

/// Cursor position marker - APC (Application Program Command) sequence.
/// This is a zero-width escape sequence that terminals ignore.
/// Components emit this at the cursor position when focused.
/// TUI finds and strips this marker, then positions the hardware cursor there.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// Helper to check if a component implements Focusable.
pub fn is_focusable(component: &ComponentRef) -> bool {
    component
        .as_any()
        .downcast_ref::<Arc<dyn Focusable>>()
        .is_some()
        || component.as_any().type_id() == std::any::TypeId::of::<Arc<dyn Focusable>>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    struct TestComponent {
        content: String,
    }

    impl Component for TestComponent {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![self.content.clone()]
        }

        fn invalidate(&self) {}

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn document_composition_retains_shared_rows_and_slices_across_blocks() {
        let first = Arc::new(vec!["one".into(), "two".into()]);
        let second = Arc::new(vec!["three".into(), "four".into()]);
        let mut document = RenderedDocument::from_shared(first.clone());
        document.append(RenderedDocument::default());
        document.append(RenderedDocument::from_shared(second.clone()));
        assert!(Arc::ptr_eq(&document.blocks[0], &first));
        assert!(Arc::ptr_eq(&document.blocks[1], &second));
        assert_eq!(document.len(), 4);
        assert_eq!(document.lines(1, 2), ["two", "three"]);
        assert_eq!(document.lines(2, usize::MAX), ["three", "four"]);
        assert!(document.lines(4, 1).is_empty());
        assert!(document.lines(usize::MAX, 1).is_empty());
        assert!(document.lines(0, 0).is_empty());
    }

    #[test]
    fn document_viewport_does_not_copy_large_history() {
        for size in [1, 1_000, 10_000] {
            let rows = Arc::new(vec!["historical row".repeat(64); size]);
            let document = RenderedDocument::from_shared(rows.clone());
            let copy = document.clone();
            assert!(Arc::ptr_eq(&copy.blocks[0], &rows));
            assert_eq!(copy.lines(size - 1, 1), [rows[size - 1].clone()]);
            assert_eq!(Arc::strong_count(&rows), 3);
        }
    }

    #[test]
    fn test_cursor_marker() {
        assert!(CURSOR_MARKER.starts_with("\x1b"));
        assert!(CURSOR_MARKER.ends_with("\x07"));
    }
}
