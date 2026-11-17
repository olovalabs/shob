//! VStack - Vertical stack layout component.
//!
//! Implements the vertical stack layout as described in `tui-plan.md`.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::layout_node::{LayoutNode, LayoutNodeProvider, StackLayoutEntry, VStackNode};

/// Options for a stack entry.
#[derive(Debug, Clone, Default)]
pub struct StackEntryOptions {
    /// Initial size on the stack's main axis. Defaults to "auto" (None = auto).
    pub basis: Option<usize>,
    /// Share of positive remaining space. Defaults to 0.
    pub grow: usize,
    /// Relative willingness to shrink when content overflows. Defaults to 1.
    pub shrink: usize,
    /// Minimum allocated size on the main axis. Defaults to 0.
    pub min_size: usize,
    /// Maximum allocated size on the main axis. None = no limit.
    pub max_size: Option<usize>,
}

impl StackEntryOptions {
    /// Create options with basis (fixed size).
    pub fn with_basis(size: usize) -> Self {
        Self {
            basis: Some(size),
            ..Default::default()
        }
    }

    /// Create options with auto sizing.
    pub fn auto() -> Self {
        Self::default()
    }

    /// Set the basis (fixed size on main axis).
    pub fn basis(mut self, size: usize) -> Self {
        self.basis = Some(size);
        self
    }

    /// Set the grow factor.
    pub fn grow(mut self, factor: usize) -> Self {
        self.grow = factor;
        self
    }

    /// Set the shrink factor.
    pub fn shrink(mut self, factor: usize) -> Self {
        self.shrink = factor;
        self
    }

    /// Set the minimum size.
    pub fn min_size(mut self, size: usize) -> Self {
        self.min_size = size;
        self
    }

    /// Set the maximum size.
    pub fn max_size(mut self, size: usize) -> Self {
        self.max_size = Some(size);
        self
    }
}

/// A child entry in a stack.
#[derive(Clone)]
pub struct StackEntry {
    pub component: Arc<dyn Component>,
    pub options: StackEntryOptions,
}

impl StackEntry {
    /// Create a new stack entry with default options.
    pub fn new(component: Arc<dyn Component>) -> Self {
        Self {
            component,
            options: StackEntryOptions::default(),
        }
    }

    /// Create a new stack entry with options.
    pub fn with_options(component: Arc<dyn Component>, options: StackEntryOptions) -> Self {
        Self { component, options }
    }

    /// Set the basis (fixed size on main axis).
    pub fn basis(mut self, size: usize) -> Self {
        self.options.basis = Some(size);
        self
    }

    /// Set the grow factor.
    pub fn grow(mut self, factor: usize) -> Self {
        self.options.grow = factor;
        self
    }

    /// Set the shrink factor.
    pub fn shrink(mut self, factor: usize) -> Self {
        self.options.shrink = factor;
        self
    }

    /// Set the minimum size.
    pub fn min_size(mut self, size: usize) -> Self {
        self.options.min_size = size;
        self
    }

    /// Set the maximum allocated size on the stack's main axis.
    pub fn max_size(mut self, size: usize) -> Self {
        self.options.max_size = Some(size);
        self
    }

    /// Convert to a StackLayoutEntry for the layout system.
    pub fn to_layout_entry(&self) -> StackLayoutEntry {
        StackLayoutEntry {
            component: self.component.clone(),
            basis: self.options.basis,
            grow: self.options.grow,
            shrink: self.options.shrink,
            min_size: self.options.min_size,
            max_size: self.options.max_size.unwrap_or(usize::MAX),
            visible: None,
        }
    }
}

/// A child that can be either a component or a stack entry.
pub enum StackChild {
    Component(Arc<dyn Component>),
    Entry(StackEntry),
}

impl From<Arc<dyn Component>> for StackChild {
    fn from(component: Arc<dyn Component>) -> Self {
        StackChild::Component(component)
    }
}

impl From<StackEntry> for StackChild {
    fn from(entry: StackEntry) -> Self {
        StackChild::Entry(entry)
    }
}

/// Options for a stack.
#[derive(Debug, Clone, Default)]
pub struct StackOptions {
    /// Gap between children in rows/columns.
    pub gap: usize,
    /// Alignment on the cross axis.
    pub align: StackAlign,
}

/// Alignment options for stack children.
#[derive(Debug, Clone, Copy, Default)]
pub enum StackAlign {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

/// VStack - Vertical stack layout.
///
/// Children are arranged from top to bottom.
#[derive(Clone)]
pub struct VStack {
    children: Arc<Mutex<Vec<StackEntry>>>,
    options: StackOptions,
}

impl VStack {
    /// Create a new empty VStack.
    pub fn new() -> Self {
        Self {
            children: Arc::new(Mutex::new(Vec::new())),
            options: StackOptions::default(),
        }
    }

    /// Create a new VStack with options.
    pub fn with_options(options: StackOptions) -> Self {
        Self {
            children: Arc::new(Mutex::new(Vec::new())),
            options,
        }
    }

    /// Create a VStack from a vector of children.
    pub fn from_children(children: Vec<StackChild>) -> Self {
        let entries: Vec<StackEntry> = children
            .into_iter()
            .map(|child| match child {
                StackChild::Component(c) => StackEntry::new(c),
                StackChild::Entry(e) => e,
            })
            .collect();

        Self {
            children: Arc::new(Mutex::new(entries)),
            options: StackOptions::default(),
        }
    }

    /// Create a VStack from a vector of stack entries.
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

    /// Calculate the intrinsic height of all children.
    #[allow(dead_code)]
    fn intrinsic_height(&self, width: usize) -> usize {
        if let Ok(children) = self.children.lock() {
            let mut total = 0;
            for (i, entry) in children.iter().enumerate() {
                let child_height = entry.component.render(width).len();
                total += child_height.max(entry.options.min_size);
                // Add gap between children
                if i > 0 && self.options.gap > 0 {
                    total += self.options.gap;
                }
            }
            total
        } else {
            0
        }
    }
}

impl Default for VStack {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for VStack {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();

        if let Ok(children) = self.children.lock() {
            // Simple rendering: render each child and concatenate
            // Full constraint-based layout would be implemented for TuiAltScreen
            for (i, entry) in children.iter().enumerate() {
                let child_lines = entry.component.render(width);
                lines.extend(child_lines);

                // Add gap between children
                if i < children.len() - 1 && self.options.gap > 0 {
                    for _ in 0..self.options.gap {
                        lines.push(String::new());
                    }
                }
            }
        }

        lines
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

impl LayoutNodeProvider for VStack {
    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::VStack(VStackNode {
            entries: self.get_layout_entries(),
            gap: self.options.gap,
            align: self.options.align,
        }))
    }
}

/// Constrained layout calculation for VStack.
///
/// Given a fixed height, distribute space among children based on their
/// basis, grow, shrink, and min_size options.
///
/// Intrinsic heights come from [`Component::render`]; callers that already
/// have a render cache should use [`layout_vstack_constrained_with`] so a
/// component measured here is not rendered a second time when it is painted.
pub fn layout_vstack_constrained(
    children: &[StackEntry],
    width: usize,
    height: usize,
    gap: usize,
) -> Vec<(Arc<dyn Component>, usize)> {
    layout_vstack_constrained_with(children, width, height, gap, |component, width| {
        component.render(width).len()
    })
}

/// Like [`layout_vstack_constrained`], but measures auto-basis children through
/// the supplied closure (usually a frame-local render cache).
///
/// Mirrors upstream routing every `measureHeight` through `renderCached`
/// (`packages/tui/src/layout.ts`).
pub(crate) fn layout_vstack_constrained_with<F>(
    children: &[StackEntry],
    width: usize,
    height: usize,
    gap: usize,
    mut measure: F,
) -> Vec<(Arc<dyn Component>, usize)>
where
    F: FnMut(&Arc<dyn Component>, usize) -> usize,
{
    let n = children.len();
    if n == 0 {
        return Vec::new();
    }

    let total_gap = if n > 1 { gap * (n - 1) } else { 0 };
    let available = height.saturating_sub(total_gap);

    // First pass: calculate basis sizes and intrinsic heights
    let mut sizes: Vec<usize> = Vec::with_capacity(n);
    let mut total_basis = 0usize;
    let mut total_grow = 0usize;
    let mut grow_indices = Vec::new();

    for (i, entry) in children.iter().enumerate() {
        // An explicit basis does not depend on intrinsic content. The
        // transcript scroll view uses basis 0 + grow 1, so measuring it here
        // used to render the complete history before its actual paint.
        let basis = entry
            .options
            .basis
            .unwrap_or_else(|| measure(&entry.component, width));
        let min_size = entry.options.min_size;
        let max_size = entry.options.max_size.unwrap_or(usize::MAX);

        let size = basis.clamp(min_size, max_size);
        sizes.push(size);
        total_basis += size;

        if entry.options.grow > 0 {
            total_grow += entry.options.grow;
            grow_indices.push(i);
        }
    }

    // Distribute remaining space or shrink
    if total_basis < available && total_grow > 0 {
        // Distribute remaining space
        let remaining = available - total_basis;
        let mut to_distribute = remaining;

        for &i in &grow_indices {
            if total_grow == 0 {
                break;
            }
            let entry = &children[i];
            let share = (remaining * entry.options.grow) / total_grow;
            let max_size = entry.options.max_size.unwrap_or(usize::MAX);
            let new_size = (sizes[i] + share).min(max_size);
            let added = new_size - sizes[i];
            sizes[i] = new_size;
            to_distribute = to_distribute.saturating_sub(added);
        }

        // Distribute any leftover
        for &i in &grow_indices {
            if to_distribute == 0 {
                break;
            }
            let entry = &children[i];
            let max_size = entry.options.max_size.unwrap_or(usize::MAX);
            if sizes[i] < max_size {
                sizes[i] += 1;
                to_distribute -= 1;
            }
        }
    } else if total_basis > available {
        // Need to shrink
        let overflow = total_basis - available;
        let mut to_shrink = overflow;

        // Find shrinkable entries
        let shrinkable: Vec<(usize, usize)> = children
            .iter()
            .enumerate()
            .filter(|(i, e)| e.options.shrink > 0 && sizes[*i] > e.options.min_size)
            .map(|(i, e)| (i, e.options.shrink))
            .collect();

        let total_shrink: usize = shrinkable.iter().map(|(_, s)| *s).sum();

        if total_shrink > 0 {
            for (i, shrink) in &shrinkable {
                if to_shrink == 0 {
                    break;
                }
                let share = (overflow * *shrink) / total_shrink;
                let min_size = children[*i].options.min_size;
                let new_size = sizes[*i].saturating_sub(share).max(min_size);
                let removed = sizes[*i] - new_size;
                sizes[*i] = new_size;
                to_shrink = to_shrink.saturating_sub(removed);
            }
        }
    }

    // Return component and size pairs
    children
        .iter()
        .zip(sizes)
        .map(|(entry, size)| (entry.component.clone(), size))
        .collect()
}
