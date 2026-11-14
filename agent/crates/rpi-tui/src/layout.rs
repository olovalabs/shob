//! Layout system for constrained rendering in TuiAltScreen.
//!
//! This module implements the layout system described in `tui-plan.md`:
//! - LayoutBox/LayoutFrame for tracking component positions
//! - Stack layout allocation with basis/grow/shrink
//! - Painting visible rows with clipping
//! - Hit testing for mouse events

use std::collections::HashMap;
use std::sync::Arc;

use super::component::{Component, RenderedDocument};
use super::hstack::HStack;
use super::layout_node::LayoutViewport;
use super::scroll_view::ScrollView;
use super::vstack::{layout_vstack_constrained_with, VStack};
use crate::ansi::{slice_by_column, visible_width, CURSOR_MARKER};
use crate::terminal_image::{
    crop_kitty_image_line, is_image_line, kitty_image_metadata, max_kitty_image_rows,
};

/// A rectangle in the layout.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl LayoutRect {
    pub fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Check if a point is inside this rectangle.
    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// Intersect with another rectangle.
    pub fn intersect(&self, other: &LayoutRect) -> LayoutRect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        LayoutRect {
            x,
            y,
            width: right.saturating_sub(x),
            height: bottom.saturating_sub(y),
        }
    }
}

/// A box in the layout tree.
#[derive(Clone)]
pub struct LayoutBox {
    /// The component this box represents.
    pub component: Arc<dyn Component>,
    /// The allocated rectangle.
    pub rect: LayoutRect,
    /// The clipping rectangle (intersection of all ancestor clips).
    pub clip: LayoutRect,
    /// Child boxes.
    pub children: Vec<LayoutBox>,
    /// Parent box index.
    pub parent_index: Option<usize>,
    /// Rendered lines (cached).
    pub lines: Option<Vec<String>>,
    /// Line offset for cursor visibility.
    pub line_offset: usize,
    /// Layer for hit testing.
    pub layer: usize,
    /// ScrollView reference if this is a scroll view.
    pub scroll_view: Option<Arc<ScrollView>>,
    /// Immutable scroll content and position captured for this layout frame.
    pub scroll_snapshot: Option<crate::scroll_view::ScrollSnapshot>,
    /// Full scrollable document, retained for placements that scrolled out of
    /// the viewport (see `paint_box`).
    pub scroll_document: Option<Arc<RenderedDocument>>,
}

impl LayoutBox {
    pub fn new(component: Arc<dyn Component>, rect: LayoutRect, clip: LayoutRect) -> Self {
        Self {
            component,
            rect,
            clip,
            children: Vec::new(),
            parent_index: None,
            lines: None,
            line_offset: 0,
            layer: 0,
            scroll_view: None,
            scroll_snapshot: None,
            scroll_document: None,
        }
    }
}

/// A complete layout frame for rendering.
#[derive(Clone)]
pub struct LayoutFrame {
    /// Root box.
    pub root: LayoutBox,
    /// Total width.
    pub width: usize,
    /// Total height.
    pub height: usize,
    /// Rendered screen lines.
    pub lines: Vec<String>,
    /// Primary scroll view (if any).
    pub primary_scroll_view: Option<Arc<ScrollView>>,
}

impl LayoutFrame {
    /// Create a new layout frame.
    pub fn new(root: LayoutBox, width: usize, height: usize) -> Self {
        Self {
            root,
            width,
            height,
            lines: vec![String::new(); height],
            primary_scroll_view: None,
        }
    }
}

/// Context for layout operations.
struct LayoutContext {
    #[allow(dead_code)]
    viewport: LayoutViewport,
    primary_scroll_view: Option<Arc<ScrollView>>,
    reuse_scroll_content: bool,
    /// Frame-local render cache: component pointer → width → rendered lines.
    ///
    /// Mirrors upstream `LayoutContext.renderCache` (`packages/tui/src/
    /// layout.ts`): inside one layout pass a component renders at most once per
    /// width, so measuring an auto-basis child does not re-render it when it is
    /// painted. `reuse_scroll_content`/`invalidate` are handled by each
    /// component's own cache; this only dedupes work within a frame, so it is
    /// dropped at the end of the frame.
    render_cache: HashMap<usize, HashMap<usize, Vec<String>>>,
}

impl LayoutContext {
    fn register_scroll_view(&mut self, sv: &Arc<ScrollView>) {
        // Keep the first primary scroll view encountered (the root transcript
        // area). Non-primary scroll views are not tracked globally.
        if sv.is_primary() && self.primary_scroll_view.is_none() {
            self.primary_scroll_view = Some(sv.clone());
        }
    }

    /// Render `component` at `width`, memoized for the current layout pass.
    ///
    /// The cache key is the `Arc` allocation address; every component is kept
    /// alive by the tree for the whole pass, so the address is stable.
    fn render_cached(&mut self, component: &Arc<dyn Component>, width: usize) -> Vec<String> {
        let key = Arc::as_ptr(component) as *const () as usize;
        if let Some(by_width) = self.render_cache.get(&key) {
            if let Some(lines) = by_width.get(&width) {
                return lines.clone();
            }
        }
        let lines = component.render(width);
        self.render_cache
            .entry(key)
            .or_default()
            .insert(width, lines.clone());
        lines
    }
}

/// Render a component tree into a layout frame with constrained layout.
pub fn render_layout_frame(root: Arc<dyn Component>, width: usize, height: usize) -> LayoutFrame {
    render_layout_frame_impl(root, width, height, false)
}

/// Render a frame where only scroll positions and non-scroll content changed.
/// Scroll views may reuse their last full child rendering.
pub(crate) fn render_layout_frame_reusing_scroll_content(
    root: Arc<dyn Component>,
    width: usize,
    height: usize,
) -> LayoutFrame {
    render_layout_frame_impl(root, width, height, true)
}

fn render_layout_frame_impl(
    root: Arc<dyn Component>,
    width: usize,
    height: usize,
    reuse_scroll_content: bool,
) -> LayoutFrame {
    let safe_width = width.max(1);
    let safe_height = height.max(1);

    let mut context = LayoutContext {
        viewport: LayoutViewport {
            width: safe_width,
            height: safe_height,
        },
        primary_scroll_view: None,
        reuse_scroll_content,
        render_cache: HashMap::new(),
    };

    let root_box = layout_component(
        &root,
        0,
        0,
        safe_width,
        Some(safe_height),
        LayoutRect::new(0, 0, safe_width, safe_height),
        &mut context,
    );

    let mut frame = LayoutFrame::new(root_box, safe_width, safe_height);
    frame.primary_scroll_view = context.primary_scroll_view;

    // Paint the root box into the screen lines
    paint_box(&frame.root, &mut frame.lines, safe_width);

    frame
}

/// Layout a single component and its children.
///
/// Recognizes `VStack` and `ScrollView` (via `as_any` downcast) and lays
/// them out with the existing constrained machinery, so stack `grow`/`shrink`/
/// `min_size` semantics are honored and the scroll view clips to its
/// allocated viewport. Any other component is treated as a leaf.
fn layout_component(
    component: &Arc<dyn Component>,
    x: usize,
    y: usize,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
    context: &mut LayoutContext,
) -> LayoutBox {
    let safe_width = width.max(1);

    // VStack: distribute the available height across children according to
    // their basis/grow/shrink/min_size (mirrors TS stack layout). This was
    // previously dead — the layout engine treated the root VStack as a leaf
    // and concatenated each child's full intrinsic height, which pushed the
    // bottom dock below the visible viewport.
    if let Some(vstack) = component.as_any().downcast_ref::<VStack>() {
        return layout_vstack(component, vstack, x, y, safe_width, height, clip, context);
    }
    if let Some(hstack) = component.as_any().downcast_ref::<HStack>() {
        return layout_hstack(component, hstack, x, y, safe_width, height, clip, context);
    }

    // ScrollView: clip the child to the allocated viewport height and render
    // only the visible window. Without this the scroll view emitted *all*
    // transcript lines, overflowing past the terminal bottom.
    if let Some(sv) = component.as_any().downcast_ref::<ScrollView>() {
        return layout_scroll_view(component, sv, x, y, safe_width, height, clip, context);
    }

    // ---- Leaf (Container/Editor/Footer/Text/...) ----
    let lines = context.render_cached(component, safe_width);

    let allocated_height = height.unwrap_or(lines.len());

    // Calculate line offset for cursor visibility
    let line_offset = if lines.len() > allocated_height && allocated_height > 0 {
        let cursor_line = lines.iter().position(|line| line.contains(CURSOR_MARKER));
        cursor_line
            .map(|idx| {
                if idx >= allocated_height {
                    idx - allocated_height + 1
                } else {
                    0
                }
            })
            .unwrap_or(0)
    } else {
        0
    };

    let rect = LayoutRect::new(x, y, safe_width, allocated_height);

    LayoutBox {
        component: component.clone(),
        rect,
        clip: clip.intersect(&rect),
        children: Vec::new(),
        parent_index: None,
        lines: Some(lines),
        line_offset,
        layer: 0,
        scroll_view: None,
        scroll_snapshot: None,
        scroll_document: None,
    }
}

/// Lay out a VStack by distributing `height` across its children.
fn layout_vstack(
    component: &Arc<dyn Component>,
    vstack: &VStack,
    x: usize,
    y: usize,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
    context: &mut LayoutContext,
) -> LayoutBox {
    let children = vstack.get_children();
    let gap = vstack.gap();
    let allocated = {
        let ctx = &mut *context;
        layout_vstack_constrained_with(
            &children,
            width,
            height.unwrap_or(0),
            gap,
            |component, w| ctx.render_cached(component, w).len(),
        )
    };

    // The box's own rect spans its full allocated height (or, when
    // unconstrained, the sum of child heights + gaps).
    let total_child_height: usize = allocated.iter().map(|(_, h)| *h).sum();
    let total_gap = if children.len() > 1 {
        gap * (children.len() - 1)
    } else {
        0
    };
    let box_height = height.unwrap_or(total_child_height + total_gap);

    let rect = LayoutRect::new(x, y, width, box_height);
    let own_clip = clip.intersect(&rect);

    let mut child_boxes = Vec::with_capacity(allocated.len());
    let mut cursor_y = y;
    for (idx, (child_component, child_height)) in allocated.iter().enumerate() {
        if idx > 0 && gap > 0 {
            cursor_y = cursor_y.saturating_add(gap);
        }
        let child_box = layout_component(
            child_component,
            x,
            cursor_y,
            width,
            Some(*child_height),
            own_clip,
            context,
        );
        // `parent_index` is the child's ordinal within its parent; `paint_box`
        // does not rely on it, but `hit_test` walks children front-to-back so
        // keeping the positional index is harmless.
        child_boxes.push(child_box);
        cursor_y = cursor_y.saturating_add(*child_height);
    }
    for (i, child) in child_boxes.iter_mut().enumerate() {
        child.parent_index = Some(i);
        child.layer = 0;
    }

    // The parent box carries no lines of its own — its children paint into
    // the screen buffer directly via `paint_box` recursion.
    let mut box_layout = LayoutBox::new(component.clone(), rect, own_clip);
    box_layout.children = child_boxes;
    box_layout
}

/// Horizontal stacks must recurse through the layout engine so scroll viewport,
/// cursor and hit-test rectangles remain tied to each child's allocated area.
fn layout_hstack(
    component: &Arc<dyn Component>,
    stack: &HStack,
    x: usize,
    y: usize,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
    context: &mut LayoutContext,
) -> LayoutBox {
    let children = stack.get_children();
    let allocated =
        layout_vstack_constrained_with(&children, width, width, stack.gap(), |child, w| {
            context
                .render_cached(child, w)
                .iter()
                .map(|line| visible_width(line))
                .max()
                .unwrap_or(0)
        });
    let mut boxes = Vec::new();
    let mut column = x;
    for (index, (child, child_width)) in allocated.into_iter().enumerate() {
        if index > 0 {
            column = column.saturating_add(stack.gap());
        }
        if child_width > 0 {
            let child_clip = clip.intersect(&LayoutRect::new(
                column,
                y,
                child_width,
                height.unwrap_or(clip.height),
            ));
            let mut child_box =
                layout_component(&child, column, y, child_width, height, child_clip, context);
            child_box.parent_index = Some(index);
            boxes.push(child_box);
        }
        column = column.saturating_add(child_width);
    }
    let box_height = height.unwrap_or_else(|| {
        boxes
            .iter()
            .map(|child| child.rect.height)
            .max()
            .unwrap_or(0)
    });
    let rect = LayoutRect::new(x, y, width, box_height);
    let mut result = LayoutBox::new(component.clone(), rect, clip.intersect(&rect));
    result.children = boxes;
    result
}

/// Lay out a ScrollView by clipping its child to the allocated viewport.
fn layout_scroll_view(
    component: &Arc<dyn Component>,
    sv: &ScrollView,
    x: usize,
    y: usize,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
    context: &mut LayoutContext,
) -> LayoutBox {
    // Track the primary scroll view globally so `get_primary_scroll_view`
    // returns the live transcript area.
    let sv_arc: Arc<ScrollView> = Arc::new(sv.clone());
    context.register_scroll_view(&sv_arc);

    let allocated_height = height.unwrap_or_else(|| sv.render(width).len());
    // Cached frames are used only for pure viewport movement or animation in
    // the bottom dock. Ordinary content updates always refresh this cache.
    let lines = if context.reuse_scroll_content {
        sv.render_with_cached_content(width, allocated_height)
    } else {
        sv.render_with_viewport(width, allocated_height)
    };

    let line_offset = if lines.len() > allocated_height && allocated_height > 0 {
        let cursor_line = lines.iter().position(|line| line.contains(CURSOR_MARKER));
        cursor_line
            .map(|idx| {
                if idx >= allocated_height {
                    idx - allocated_height + 1
                } else {
                    0
                }
            })
            .unwrap_or(0)
    } else {
        0
    };

    let rect = LayoutRect::new(x, y, width, allocated_height);

    LayoutBox {
        component: component.clone(),
        rect,
        clip: clip.intersect(&rect),
        children: Vec::new(),
        parent_index: None,
        lines: Some(lines),
        line_offset,
        layer: 0,
        scroll_view: Some(sv_arc),
        scroll_snapshot: sv.snapshot(width),
        scroll_document: sv.rendered_document(),
    }
}

/// Translate a box and its children by a delta.
#[allow(dead_code)]
fn translate_box(lbox: &mut LayoutBox, delta_y: i32) {
    if delta_y >= 0 {
        lbox.rect.y += delta_y as usize;
    } else {
        lbox.rect.y = lbox.rect.y.saturating_sub((-delta_y) as usize);
    }
    for child in &mut lbox.children {
        translate_box(child, delta_y);
    }
}

/// Update clips for a box and its children.
#[allow(dead_code)]
fn update_clips(lbox: &mut LayoutBox, parent_clip: LayoutRect) {
    lbox.clip = parent_clip.intersect(&lbox.rect);
    let child_clip = lbox.clip;
    for child in &mut lbox.children {
        update_clips(child, child_clip);
    }
}

/// Paint a box and its children into the screen buffer.
fn paint_box(lbox: &LayoutBox, screen: &mut [String], total_width: usize) {
    if let Some(ref lines) = lbox.lines {
        let offset = lbox.line_offset;

        // Calculate visible row range
        let first_row = lbox.rect.y.max(lbox.clip.y);
        let last_row = (lbox.rect.y + lbox.rect.height)
            .min(lbox.clip.y + lbox.clip.height)
            .min(screen.len());

        for row in first_row..last_row {
            if row >= screen.len() {
                break;
            }

            let source_idx = offset + row.saturating_sub(lbox.rect.y);
            if source_idx >= lines.len() {
                break;
            }

            let source_line = &lines[source_idx];

            // A Kitty placement is anchored to a screen cell: the terminal
            // paints `r` rows downward from it and cannot see this clip rect.
            // Crop the placement to the rows that fit, otherwise an image near
            // the end of the viewport paints over the dock below it.
            let clipped = clip_placement_below(
                source_line,
                row,
                lbox.clip.y + lbox.clip.height,
                screen.len(),
            );
            let line = clipped.as_deref().unwrap_or(source_line.as_str());

            // Composite the line into the screen
            if lbox.rect.x == 0 && lbox.rect.width >= total_width {
                // Fast path: full-width box at left edge
                screen[row] = line.to_string();
            } else {
                // Composite at the correct position
                screen[row] = composite_tui_line(
                    &screen[row],
                    line,
                    lbox.rect.x,
                    lbox.rect.width,
                    total_width,
                );
            }
        }
    }

    // Paint children
    for child in &lbox.children {
        paint_box(child, screen, total_width);
    }

    // A placement that scrolled above the viewport has to be re-anchored to the
    // first visible row: its escape lives on the image's first row, which the
    // visible slice no longer contains, so the image would vanish while most of
    // it is still on screen. Paint the cropped remainder here, after the
    // children, so it lands on top of the (blank) placeholder rows.
    paint_scrolled_placement(lbox, screen, total_width);
}

/// Crop a placement so it cannot paint below `clip_bottom`.
///
/// Returns `None` when the placement fits and the line can be used as is.
fn clip_placement_below(
    line: &str,
    row: usize,
    clip_bottom: usize,
    screen_len: usize,
) -> Option<String> {
    let metadata = kitty_image_metadata(line)?;
    let bottom = clip_bottom.min(screen_len);
    let room = bottom.saturating_sub(row).min(usize::from(u16::MAX)) as u16;
    let visible = metadata.rows.min(room);
    (visible < metadata.rows).then(|| crop_kitty_image_line(line, 0, visible))
}

/// Paint the visible remainder of a placement whose anchor row is above the
/// first row this box actually paints.
///
/// That happens when the placement scrolled above the viewport, and also when
/// an ancestor clip hides the top of an unscrolled viewport: either way the
/// escape lives on a row `paint_box` never emits, so the image would vanish
/// while most of it is still on screen.
fn paint_scrolled_placement(lbox: &LayoutBox, screen: &mut [String], total_width: usize) {
    let (Some(scroll_view), Some(document)) =
        (lbox.scroll_view.as_ref(), lbox.scroll_document.as_ref())
    else {
        return;
    };
    let scroll_top = scroll_view.scroll_top();
    let top = lbox.rect.y.max(lbox.clip.y);
    let bottom = (lbox.clip.y + lbox.clip.height).min(screen.len());
    if lbox.rect.height == 0 || bottom <= top {
        return;
    }
    // An ancestor clip may hide the first rows of the viewport; the placement
    // has to skip those as well. `paint_box` starts at `clip_offset` rows into
    // the viewport slice, so the first *visible* document row is
    // `scroll_top + line_offset + clip_offset` — a placement anchored between
    // `scroll_top` and that boundary still has rows on screen even though the
    // paint loop never saw its escape.
    let clip_offset = top.saturating_sub(lbox.rect.y);
    let visible_source_top = scroll_top
        .saturating_add(lbox.line_offset)
        .saturating_add(clip_offset);
    // Nothing can be above the first row of the document: the search range is
    // empty and the loop below is a no-op, which is also the common unscrolled,
    // unclipped case.
    // Rows above the viewport can only reach into it if they belong to a
    // placement no taller than the tallest one registered, so the search is
    // bounded instead of scanning the whole history.
    let min_row = visible_source_top.saturating_sub(max_kitty_image_rows());
    for image_row in (min_row..visible_source_top).rev() {
        let Some(line) = document.line(image_row) else {
            continue;
        };
        if !is_image_line(line) {
            continue;
        }
        let Some(metadata) = kitty_image_metadata(line) else {
            continue;
        };
        let hidden = (visible_source_top - image_row).min(usize::from(u16::MAX)) as u16;
        if hidden >= metadata.rows {
            continue;
        }
        let viewport_rows = (bottom - top).min(usize::from(u16::MAX)) as u16;
        let visible = metadata.rows.saturating_sub(hidden).min(viewport_rows);
        let cropped = crop_kitty_image_line(line, hidden, visible);
        screen[top] = if lbox.rect.x == 0 && lbox.rect.width >= total_width {
            cropped
        } else {
            composite_tui_line(
                &screen[top],
                &cropped,
                lbox.rect.x,
                lbox.rect.width,
                total_width,
            )
        };
        return;
    }
}

/// Composite an overlay line onto a base line at a given column position.
pub fn composite_tui_line(
    base: &str,
    overlay: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    let base_width = visible_width(base);
    let overlay_actual_width = visible_width(overlay);

    // If overlay is empty or starts beyond total width, return base
    if overlay_actual_width == 0 || start_col >= total_width {
        return base.to_string();
    }

    // Calculate the portion of overlay that fits
    let effective_width = overlay_width.min(total_width.saturating_sub(start_col));

    // Slice or pad overlay to effective width
    let overlay_padded = if overlay_actual_width < effective_width {
        // Pad right
        format!(
            "{}{}",
            overlay,
            " ".repeat(effective_width - overlay_actual_width)
        )
    } else if overlay_actual_width > effective_width {
        // Truncate
        crate::line_diff::slice(overlay, 0, effective_width)
            .unwrap_or_else(|| slice_by_column(overlay, 0, effective_width, true))
    } else {
        overlay.to_string()
    };

    // Build the composite line
    let before_width = start_col.min(base_width);
    let after_start = start_col + effective_width;

    // Get before portion from base
    let before = if before_width > 0 {
        crate::line_diff::slice(base, 0, before_width)
            .unwrap_or_else(|| slice_by_column(base, 0, before_width, true))
    } else {
        String::new()
    };

    // Get after portion from base
    let after = if after_start < base_width {
        let width = base_width.saturating_sub(after_start);
        crate::line_diff::slice(base, after_start, width)
            .unwrap_or_else(|| slice_by_column(base, after_start, width, true))
    } else {
        String::new()
    };

    // Combine with resets to prevent style leakage
    const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

    // Calculate padding
    let before_pad = if visible_width(&before) < start_col {
        " ".repeat(start_col - visible_width(&before))
    } else {
        String::new()
    };

    let after_pad = if after_start < total_width {
        let expected_after_width = total_width.saturating_sub(after_start);
        let actual_after_width = visible_width(&after);
        if actual_after_width < expected_after_width {
            " ".repeat(expected_after_width - actual_after_width)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    format!(
        "{}{}{}{}{}{}{}",
        before, before_pad, SEGMENT_RESET, overlay_padded, SEGMENT_RESET, after, after_pad
    )
}

/// Hit test to find the deepest box at a given coordinate.
pub fn hit_test(frame: &LayoutFrame, x: usize, y: usize) -> Option<&LayoutBox> {
    hit_test_box(&frame.root, x, y)
}

fn hit_test_box(lbox: &LayoutBox, x: usize, y: usize) -> Option<&LayoutBox> {
    // Check if point is in clip
    if !lbox.clip.contains(x, y) {
        return None;
    }

    // Check if point is in rect
    if !lbox.rect.contains(x, y) {
        return None;
    }

    // Check children first (front to back)
    for child in lbox.children.iter().rev() {
        if let Some(hit) = hit_test_box(child, x, y) {
            return Some(hit);
        }
    }

    // Return this box if no child was hit
    Some(lbox)
}

/// Find all scroll views at a given coordinate.
pub fn get_scroll_views_at(frame: &LayoutFrame, x: usize, y: usize) -> Vec<Arc<ScrollView>> {
    let mut result = Vec::new();
    collect_scroll_views(&frame.root, x, y, &mut result);
    result
}

fn collect_scroll_views(lbox: &LayoutBox, x: usize, y: usize, result: &mut Vec<Arc<ScrollView>>) {
    if !lbox.clip.contains(x, y) || !lbox.rect.contains(x, y) {
        return;
    }

    if let Some(ref sv) = lbox.scroll_view {
        result.push(sv.clone());
    }

    for child in &lbox.children {
        collect_scroll_views(child, x, y, result);
    }
}

/// Extract cursor position from rendered lines.
pub fn extract_cursor_position(lines: &[String], height: usize) -> Option<(usize, usize)> {
    let viewport_top = lines.len().saturating_sub(height);

    for row in (viewport_top..lines.len()).rev() {
        let line = &lines[row];
        if let Some(marker_idx) = line.find(CURSOR_MARKER) {
            let before = &line[..marker_idx];
            let col = visible_width(before);
            return Some((row, col));
        }
    }

    None
}

/// Strip cursor markers from lines.
pub fn strip_cursor_markers(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.replace(CURSOR_MARKER, ""))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn overlay_composition_pads_to_its_hit_column_and_keeps_existing_styles() {
        let output = super::composite_tui_line("\x1b[32mabc\x1b[0m", "Jump", 10, 4, 20);
        assert_eq!(crate::ansi::strip_ansi(&output), "abc       Jump      ");
        assert!(output.contains("\x1b[32mabc"));
        let output = super::composite_tui_line(
            "\x1b[31mabcdefghijklmnop\x1b[0m",
            "\x1b[44mJump\x1b[49m",
            4,
            4,
            16,
        );
        assert!(output.contains("\x1b[31mabcd"));
        assert!(output.contains("\x1b[31mijklmnop"));
        assert_eq!(crate::ansi::strip_ansi(&output), "abcdJumpijklmnop");
    }

    use super::*;
    use crate::{
        Container, Editor, EditorOptions, EditorStyle, Focusable, ScrollView, ScrollViewOptions,
        StackChild, StackEntry, Text, VStack,
    };

    #[test]
    fn test_layout_rect_contains() {
        let rect = LayoutRect::new(5, 5, 10, 10);
        assert!(rect.contains(5, 5)); // Top-left corner
        assert!(rect.contains(10, 10)); // Inside
        assert!(!rect.contains(4, 5)); // Left of
        assert!(!rect.contains(15, 5)); // Right of
        assert!(!rect.contains(5, 15)); // Below
    }

    #[test]
    fn test_layout_rect_intersect() {
        let a = LayoutRect::new(0, 0, 10, 10);
        let b = LayoutRect::new(5, 5, 10, 10);
        let c = a.intersect(&b);
        assert_eq!(c.x, 5);
        assert_eq!(c.y, 5);
        assert_eq!(c.width, 5);
        assert_eq!(c.height, 5);
    }

    #[test]
    fn test_composite_tui_line() {
        let base = "Hello, World!";
        let overlay = "Rust";
        let result = composite_tui_line(base, overlay, 7, 4, 20);
        assert!(result.contains("Hello,"));
        assert!(result.contains("Rust"));
    }

    #[test]
    fn test_render_layout_frame() {
        let text = Arc::new(Text::new("Hello, World!", 0, 0));
        let frame = render_layout_frame(text, 20, 10);
        assert_eq!(frame.width, 20);
        assert_eq!(frame.height, 10);
        assert!(!frame.lines.is_empty());
    }

    /// Regression for the leaf-only layout bug: a root `VStack[ scrollview
    /// (grow1), dock ]` must pin the dock (editor borders + footer) to the
    /// bottom of the viewport and clip the scrollview to the remainder. Before
    /// the fix the dock was pushed below the visible rows and the screen was
    /// black where the editor should be.
    #[test]
    fn test_constrained_layout_dock_pinned_to_bottom() {
        // Tall transcript (many lines) so the scrollview's intrinsic height
        // exceeds the viewport — it must be clipped, not overflow.
        let transcript = Arc::new(Container::new());
        for i in 0..40 {
            transcript.add_child(Arc::new(Text::new(&format!("line {i:02}"), 0, 0)));
        }
        let scroll = Arc::new(ScrollView::new(
            transcript,
            // FollowMode::None so the scrollview doesn't jump to the end
            // (which would push the first 30 lines off-screen) — we want to
            // assert the top of the transcript is visible, i.e. clipped, not
            // that it rendered *all* 40 lines.
            ScrollViewOptions {
                follow: crate::FollowMode::None,
                ..Default::default()
            },
        ));

        let editor = Arc::new(Editor::new(
            EditorOptions::default(),
            EditorStyle::default(),
            Arc::new(crate::Keybindings::new()),
        ));
        editor.set_focused(true);
        let editor_container = Arc::new(Container::new());
        editor_container.add_child(editor.clone());
        let footer = Arc::new(Text::new("FOOTER LINE", 0, 0));

        let dock = Arc::new(VStack::from_children(vec![
            StackChild::Entry(
                StackEntry::new(editor_container.clone())
                    .shrink(0)
                    .min_size(3),
            ),
            StackChild::Entry(StackEntry::new(footer.clone())),
        ]));

        let root = VStack::from_children(vec![
            StackChild::Entry(
                StackEntry::new(scroll.clone())
                    .basis(0)
                    .grow(1)
                    .shrink(1)
                    .min_size(1),
            ),
            StackChild::Entry(StackEntry::new(dock).shrink(1)),
        ]);

        // 40-col wide, 10-row tall viewport.
        let frame = render_layout_frame(Arc::new(root), 40, 10);

        // The screen buffer must be exactly the viewport height.
        assert_eq!(
            frame.lines.len(),
            10,
            "frame should be clipped to viewport height"
        );

        // The dock (editor borders + footer) must occupy the bottom rows of
        // the painted screen — the editor renders 3 rows (top border, content,
        // bottom border) + the footer 1 row = 4 dock rows. The bottom-most
        // painted row must be the footer.
        let bottom = frame.lines.last().expect("non-empty frame");
        assert!(
            bottom.contains("FOOTER"),
            "footer should be the last painted row; got: {bottom:?}"
        );

        // The editor's top border (a run of `─`) must appear on-screen just
        // above the footer region. Borders are colored via the theme border
        // color, so check for the `─` glyph rather than a raw repeat.
        let on_screen: String = frame.lines.join("\n");
        assert!(
            on_screen.contains('─'),
            "editor top border `─` should be visible on-screen; rendered:\n{on_screen}"
        );

        // The scrollview must NOT paint all 40 transcript lines — it must be
        // clipped to the top rows. "line 39" (the last) must be absent.
        assert!(
            !on_screen.contains("line 39"),
            "scrollview should be clipped to the viewport, not render all 40 lines"
        );

        // Primary scroll view should be tracked so get_primary_scroll_view /
        // frame.primary_scroll_view is populated for the scroll routing.
        // (The default ScrollView is non-primary; skip that assertion here and
        // rely on the is_primary branch in layout_scroll_view.)
    }
}

#[cfg(test)]
mod scroll_layout_tests {
    use super::*;
    use crate::component::Component;
    use crate::{Container, ScrollView, ScrollViewOptions, Text};
    use std::any::Any;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingTranscript {
        renders: Arc<AtomicUsize>,
    }

    impl Component for CountingTranscript {
        fn render(&self, _width: usize) -> Vec<String> {
            self.renders.fetch_add(1, Ordering::SeqCst);
            (0..100).map(|i| format!("row {i:03}")).collect()
        }

        fn invalidate(&self) {}

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn tall() -> (Arc<ScrollView>, Arc<Container>) {
        let inner = Arc::new(Container::new());
        for i in 0..50 {
            inner.add_child(Arc::new(Text::new(format!("row {i:02}"), 0, 0)));
        }
        let sv = Arc::new(ScrollView::new(
            inner.clone(),
            ScrollViewOptions {
                follow: crate::FollowMode::End,
                primary: true,
                ..Default::default()
            },
        ));
        (sv, inner)
    }

    #[test]
    fn layout_scroll_view_respects_scroll_top() {
        let (sv, _) = tall();
        let root: Arc<dyn Component> = sv.clone();
        // Initial: following end → tail visible.
        let f1 = render_layout_frame(root.clone(), 40, 10);
        assert!(
            f1.lines[0].contains("row 40"),
            "tail visible: {:?}",
            f1.lines[0]
        );
        let snapshot = f1.root.scroll_snapshot.as_ref().unwrap();
        assert_eq!(snapshot.scroll_top, 40);
        assert_eq!(snapshot.content_width, 40);
        // Scroll up, then re-layout: older rows must appear.
        sv.scroll_by(-10);
        let f2 = render_layout_frame(root, 40, 10);
        assert_eq!(f2.root.scroll_snapshot.as_ref().unwrap().scroll_top, 30);
        assert_eq!(snapshot.scroll_top, 40);
        assert!(snapshot.document.lines(40, 1)[0].contains("row 40"));
        assert!(
            f2.lines[0].contains("row 30"),
            "after scroll-up, row 30 top: {:?}",
            f2.lines[0]
        );
    }

    #[test]
    fn scroll_frames_reuse_rendered_transcript() {
        let renders = Arc::new(AtomicUsize::new(0));
        let transcript = Arc::new(CountingTranscript {
            renders: renders.clone(),
        });
        let scroll = Arc::new(ScrollView::new(
            transcript,
            ScrollViewOptions {
                follow: crate::FollowMode::End,
                primary: true,
                ..Default::default()
            },
        ));
        let root: Arc<dyn Component> = Arc::new(VStack::from_children(vec![
            crate::StackChild::Entry(
                crate::StackEntry::new(scroll.clone())
                    .basis(0)
                    .grow(1)
                    .min_size(1),
            ),
            crate::StackChild::Entry(crate::StackEntry::new(Arc::new(Text::new("dock", 0, 0)))),
        ]));

        render_layout_frame(root.clone(), 40, 10);
        assert_eq!(renders.load(Ordering::SeqCst), 1);

        scroll.scroll_by(-1);
        render_layout_frame_reusing_scroll_content(root.clone(), 40, 10);
        assert_eq!(
            renders.load(Ordering::SeqCst),
            1,
            "pure scrolling must not rebuild the transcript",
        );

        render_layout_frame(root.clone(), 40, 10);
        assert_eq!(
            renders.load(Ordering::SeqCst),
            2,
            "ordinary frames must refresh mutable transcript content",
        );

        render_layout_frame_reusing_scroll_content(root, 60, 10);
        assert_eq!(
            renders.load(Ordering::SeqCst),
            3,
            "a width change must invalidate wrapped transcript lines",
        );
    }

    /// A ScrollView can sit inside a box whose clip starts below its top edge.
    /// The paint loop then skips the rows above the clip — including the row
    /// that carries a placement's escape — so the placement has to be
    /// re-anchored from the first *visible* source row, not from `scroll_top`.
    #[test]
    fn a_placement_above_an_ancestor_clip_is_reanchored_to_the_first_visible_row() {
        const ID: u32 = 9_903;
        crate::terminal_image::register_kitty_image(crate::terminal_image::KittyImageMetadata {
            id: ID,
            columns: 20,
            rows: 8,
            width_px: 400,
            height_px: 800,
        });

        // The image is anchored on document row 5 and covers rows 5..13.
        let mut rows: Vec<String> = (0..30).map(|row| format!("row {row}")).collect();
        rows[5] = format!("\x1b_Ga=T,f=100,C=1,q=2,i={ID},p=1,c=20,r=8,m=0;AAAA\x1b\\");
        for row in &mut rows[6..13] {
            row.clear();
        }

        struct FixedRows(Vec<String>);
        impl Component for FixedRows {
            fn render(&self, _width: usize) -> Vec<String> {
                self.0.clone()
            }
            fn invalidate(&self) {}
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let scroll = Arc::new(ScrollView::new(
            Arc::new(FixedRows(rows)),
            ScrollViewOptions {
                follow: crate::FollowMode::None,
                ..Default::default()
            },
        ));
        // Two passes: the first sizes the document, the second renders the
        // viewport at the scroll position the test wants.
        let _ = scroll.render_with_viewport(40, 10);
        scroll.scroll_to(5);
        let viewport = scroll.render_with_viewport(40, 10);

        // The box starts at row 0 but an ancestor clips it to rows 3..10, so
        // rows 0..3 of the viewport (document rows 5..8) are never painted.
        let mut lbox = LayoutBox::new(
            Arc::new(Text::new("", 0, 0)),
            LayoutRect::new(0, 0, 40, 10),
            LayoutRect::new(0, 3, 40, 7),
        );
        lbox.lines = Some(viewport);
        lbox.scroll_view = Some(scroll.clone());
        lbox.scroll_document = scroll.rendered_document();

        let mut screen = vec![String::new(); 10];
        paint_box(&lbox, &mut screen, 40);

        assert!(
            screen[3].contains(",y=300,h=500,r=5;"),
            "the placement must resume at the first visible row with the rows \
             hidden by the ancestor clip cropped away, got {:?}",
            screen[3]
        );
        assert!(
            screen[4..].iter().all(|row| !row.contains("\x1b_G")),
            "the placement must be emitted once, on the row it is anchored to"
        );
    }

    /// The same ancestor clip can hide the anchor of an image that never
    /// scrolled: the boundary is the first *visible* source row, so
    /// `scroll_top == 0` must not short-circuit the re-anchor.
    #[test]
    fn a_placement_under_an_ancestor_clip_is_reanchored_without_scrolling() {
        const ID: u32 = 9_904;
        crate::terminal_image::register_kitty_image(crate::terminal_image::KittyImageMetadata {
            id: ID,
            columns: 20,
            rows: 8,
            width_px: 400,
            height_px: 800,
        });

        // The image is anchored on document row 0 and covers rows 0..8.
        let mut rows: Vec<String> = (0..30).map(|row| format!("row {row}")).collect();
        rows[0] = format!("\x1b_Ga=T,f=100,C=1,q=2,i={ID},p=1,c=20,r=8,m=0;AAAA\x1b\\");
        for row in &mut rows[1..8] {
            row.clear();
        }

        struct FixedRows(Vec<String>);
        impl Component for FixedRows {
            fn render(&self, _width: usize) -> Vec<String> {
                self.0.clone()
            }
            fn invalidate(&self) {}
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let scroll = Arc::new(ScrollView::new(
            Arc::new(FixedRows(rows)),
            ScrollViewOptions {
                follow: crate::FollowMode::None,
                ..Default::default()
            },
        ));
        let viewport = scroll.render_with_viewport(40, 10);
        assert_eq!(scroll.scroll_top(), 0, "the viewport must not be scrolled");

        // rect rows 0..10, clip rows 3..10: the image's first three rows
        // (document rows 0..3) are never painted.
        let mut lbox = LayoutBox::new(
            Arc::new(Text::new("", 0, 0)),
            LayoutRect::new(0, 0, 40, 10),
            LayoutRect::new(0, 3, 40, 7),
        );
        lbox.lines = Some(viewport);
        lbox.scroll_view = Some(scroll.clone());
        lbox.scroll_document = scroll.rendered_document();

        let mut screen = vec![String::new(); 10];
        paint_box(&lbox, &mut screen, 40);

        assert!(
            screen[3].contains(",y=300,h=500,r=5;"),
            "the placement must resume at the first visible row with the rows \
             hidden by the ancestor clip cropped away, got {:?}",
            screen[3]
        );
        assert!(
            screen[4..].iter().all(|row| !row.contains("\x1b_G")),
            "the placement must be emitted once, on the row it is anchored to"
        );
    }
}
