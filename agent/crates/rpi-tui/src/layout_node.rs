//! Layout node system for exposing component layout structure.
//!
//! This allows VStack, HStack, and ScrollView to expose their internal structure
//! to the layout system for proper constrained layout.

use std::sync::Arc;

use super::component::Component;
use super::vstack::StackAlign;

/// A viewport for layout calculations.
#[derive(Debug, Clone, Copy)]
pub struct LayoutViewport {
    pub width: usize,
    pub height: usize,
}

impl Default for LayoutViewport {
    fn default() -> Self {
        Self {
            width: 80,
            height: 24,
        }
    }
}

/// Layout node types.
#[derive(Clone)]
pub enum LayoutNode {
    /// A vertical stack.
    VStack(VStackNode),
    /// A horizontal stack.
    HStack(HStackNode),
    /// A scroll view.
    Scroll(ScrollNode),
}

/// VStack layout node.
#[derive(Clone)]
pub struct VStackNode {
    pub entries: Vec<StackLayoutEntry>,
    pub gap: usize,
    pub align: StackAlign,
}

/// HStack layout node.
#[derive(Clone)]
pub struct HStackNode {
    pub entries: Vec<StackLayoutEntry>,
    pub gap: usize,
    pub align: StackAlign,
}

/// Scroll view layout node.
#[derive(Clone)]
pub struct ScrollNode {
    pub component: Arc<dyn Component>,
    /// A callback to get the scroll state during layout
    pub get_scroll_top: fn() -> usize,
}

impl ScrollNode {
    /// Create a new scroll node with a component.
    pub fn new(component: Arc<dyn Component>) -> Self {
        Self {
            component,
            get_scroll_top: || 0,
        }
    }

    /// Create a scroll node with a scroll top getter.
    pub fn with_scroll_top(component: Arc<dyn Component>, get_scroll_top: fn() -> usize) -> Self {
        Self {
            component,
            get_scroll_top,
        }
    }
}

/// A layout entry in a stack.
#[derive(Clone)]
pub struct StackLayoutEntry {
    pub component: Arc<dyn Component>,
    pub basis: Option<usize>,
    pub grow: usize,
    pub shrink: usize,
    pub min_size: usize,
    pub max_size: usize,
    pub visible: Option<fn(LayoutViewport) -> bool>,
}

impl StackLayoutEntry {
    /// Create a new stack layout entry.
    pub fn new(component: Arc<dyn Component>) -> Self {
        Self {
            component,
            basis: None,
            grow: 0,
            shrink: 1,
            min_size: 0,
            max_size: usize::MAX,
            visible: None,
        }
    }

    /// Create from a component with specific options.
    pub fn with_options(
        component: Arc<dyn Component>,
        basis: Option<usize>,
        grow: usize,
        shrink: usize,
        min_size: usize,
        max_size: usize,
    ) -> Self {
        Self {
            component,
            basis,
            grow,
            shrink,
            min_size,
            max_size,
            visible: None,
        }
    }
}

/// Trait for components that expose a layout node.
pub trait LayoutNodeProvider: Component {
    /// Get the layout node if this component has one.
    fn layout_node(&self) -> Option<LayoutNode>;
}

/// Calculate sizes for stack entries given available space.
pub fn allocate_stack_sizes(
    entries: &[StackLayoutEntry],
    intrinsic_sizes: &[usize],
    available_size: Option<usize>,
    gap: usize,
) -> Vec<usize> {
    let n = entries.len();
    if n == 0 {
        return Vec::new();
    }

    let total_gap = if n > 1 { gap * (n - 1) } else { 0 };

    // First pass: calculate basis sizes
    let mut sizes: Vec<usize> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let basis = entry
                .basis
                .unwrap_or_else(|| intrinsic_sizes.get(i).copied().unwrap_or(0));
            basis.clamp(entry.min_size, entry.max_size)
        })
        .collect();

    if let Some(available) = available_size {
        let content_available = available.saturating_sub(total_gap);
        let total_basis: usize = sizes.iter().sum();

        if total_basis < content_available {
            // Distribute remaining space to growing entries
            let remaining = content_available - total_basis;
            let total_grow: usize = entries.iter().map(|e| e.grow).sum();

            if total_grow > 0 {
                let mut to_distribute = remaining;

                // First pass: distribute proportionally
                for (i, entry) in entries.iter().enumerate() {
                    if entry.grow > 0 && to_distribute > 0 {
                        let share = (remaining * entry.grow / total_grow).min(to_distribute);
                        let new_size = (sizes[i] + share).min(entry.max_size);
                        let added = new_size - sizes[i];
                        sizes[i] = new_size;
                        to_distribute -= added;
                    }
                }

                // Second pass: distribute leftover
                for (i, entry) in entries.iter().enumerate() {
                    if to_distribute == 0 {
                        break;
                    }
                    if entry.grow > 0 && sizes[i] < entry.max_size {
                        sizes[i] += 1;
                        to_distribute -= 1;
                    }
                }
            }
        } else if total_basis > content_available {
            // Need to shrink
            let overflow = total_basis - content_available;
            let mut to_shrink = overflow;

            // Find shrinkable entries
            let shrinkable: Vec<(usize, usize)> = entries
                .iter()
                .enumerate()
                .filter(|(i, e)| e.shrink > 0 && sizes[*i] > e.min_size)
                .map(|(i, e)| (i, e.shrink * sizes[i].max(1)))
                .collect();

            let total_shrink: usize = shrinkable.iter().map(|(_, s)| *s).sum();

            if total_shrink > 0 {
                for (i, weight) in &shrinkable {
                    if to_shrink == 0 {
                        break;
                    }
                    let share = (overflow * weight / total_shrink).min(to_shrink);
                    let entry = &entries[*i];
                    let new_size = sizes[*i].saturating_sub(share).max(entry.min_size);
                    let removed = sizes[*i] - new_size;
                    sizes[*i] = new_size;
                    to_shrink -= removed;
                }
            }
        }
    }

    sizes
}

/// Filter visible stack entries based on viewport.
pub fn visible_stack_entries(
    entries: &[StackLayoutEntry],
    viewport: LayoutViewport,
) -> Vec<StackLayoutEntry> {
    entries
        .iter()
        .filter(|entry| entry.visible.map(|v| v(viewport)).unwrap_or(true))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Text;

    #[test]
    fn test_allocate_stack_sizes_auto() {
        let entries = vec![
            StackLayoutEntry::new(Arc::new(Text::new("A", 0, 0))),
            StackLayoutEntry::new(Arc::new(Text::new("B", 0, 0))),
        ];

        let intrinsic = vec![3, 3]; // Both have 3 lines
        let sizes = allocate_stack_sizes(&entries, &intrinsic, None, 0);
        assert_eq!(sizes, vec![3, 3]);
    }

    #[test]
    fn test_allocate_stack_sizes_grow() {
        let entries = vec![
            StackLayoutEntry::with_options(
                Arc::new(Text::new("A", 0, 0)),
                Some(2),
                1,
                1,
                0,
                usize::MAX,
            ),
            StackLayoutEntry::with_options(
                Arc::new(Text::new("B", 0, 0)),
                Some(2),
                1,
                1,
                0,
                usize::MAX,
            ),
        ];

        let intrinsic = vec![2, 2];
        let sizes = allocate_stack_sizes(&entries, &intrinsic, Some(10), 0);
        assert_eq!(sizes.iter().sum::<usize>(), 10);
    }

    #[test]
    fn test_allocate_stack_sizes_shrink() {
        let entries = vec![
            StackLayoutEntry::with_options(
                Arc::new(Text::new("A", 0, 0)),
                Some(10),
                0,
                1,
                2,
                usize::MAX,
            ),
            StackLayoutEntry::with_options(
                Arc::new(Text::new("B", 0, 0)),
                Some(10),
                0,
                1,
                2,
                usize::MAX,
            ),
        ];

        let intrinsic = vec![10, 10];
        let sizes = allocate_stack_sizes(&entries, &intrinsic, Some(10), 0);
        // Total should be 10, each gets at least min_size
        assert!(sizes[0] >= 2);
        assert!(sizes[1] >= 2);
        assert_eq!(sizes.iter().sum::<usize>(), 10);
    }
}
