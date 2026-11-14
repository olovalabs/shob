//! Text selection for the fullscreen (alternate-screen) TUI.
//!
//! Mirrors upstream pi's `tui-alt-screen.ts` selection model: an anchor and a
//! focus live in *content* coordinates (row within the scroll view's cached
//! document, column within that row) while the drag stays inside the scroll
//! view, so a selection keeps pointing at the same text while the transcript
//! scrolls underneath. Points outside any scroll view fall back to raw screen
//! coordinates (upstream's non-scroll path); the two spaces never mix — a
//! selection anchored in one space ignores motion resolved into the other.
//!
//! Highlighting is applied at render time by reversing video over the selected
//! column span; extraction slices the cached document rows by visible column,
//! so what lands on the clipboard is exactly what was highlighted.

use std::sync::{Arc, Mutex};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::ansi::{strip_ansi, visible_width};
use crate::layout::{LayoutBox, LayoutFrame};
use crate::line_diff;
use crate::scroll_view::ScrollView;

/// One endpoint of a selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionPoint {
    /// Row within the scroll view's document (`in_scroll`), or the screen row.
    pub row: usize,
    /// Terminal cell column within that row (or the EOF boundary below the
    /// document). `bounds()` converts the later cell to an exclusive boundary.
    pub col: usize,
    /// Whether this point was resolved inside the anchored scroll view.
    pub in_scroll: bool,
}

/// The active selection: where the drag started and where it currently ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub anchor: SelectionPoint,
    pub focus: SelectionPoint,
}

/// Selection state owned by the fullscreen TUI.
#[derive(Default)]
pub struct SelectionState {
    selection: Mutex<Option<Selection>>,
    /// The scroll view the drag was anchored in, if any. Drag and release
    /// events resolve their position against THIS view (clamped to its
    /// viewport), not against whatever view sits under the pointer now —
    /// otherwise a drag that crosses the dock would jump coordinate spaces.
    anchor_scroll: Mutex<Option<Arc<ScrollView>>>,
    /// A plain click resumes the follow mode that pressing temporarily paused.
    /// A real selection remains detached until the user scrolls to the end.
    resume_follow_on_click: Mutex<bool>,
    /// Button is currently held (a `Down` without its `Up` yet).
    press_active: Mutex<bool>,
    /// Screen cell of Down, independent of document movement while pressed.
    press_origin: Mutex<Option<(u16, u16)>>,
    /// The pointer moved with the button held. Only a real drag selects and
    /// copies; a plain click collapses the selection.
    dragged: Mutex<bool>,
}

impl SelectionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current selection, if any.
    pub fn selection(&self) -> Option<Selection> {
        self.selection.lock().ok().and_then(|s| s.clone())
    }

    pub fn clear(&self) {
        let resume_cancelled_click = self
            .press_active
            .lock()
            .map(|active| *active)
            .unwrap_or(false)
            && !self.dragged()
            && self
                .resume_follow_on_click
                .lock()
                .map(|resume| *resume)
                .unwrap_or(false);
        let view = resume_cancelled_click
            .then(|| self.anchor_scroll_view())
            .flatten();
        if let Ok(mut sel) = self.selection.lock() {
            *sel = None;
        }
        if let Ok(mut view) = self.anchor_scroll.lock() {
            *view = None;
        }
        if let Ok(mut resume) = self.resume_follow_on_click.lock() {
            *resume = false;
        }
        if let Ok(mut active) = self.press_active.lock() {
            *active = false;
        }
        if let Ok(mut origin) = self.press_origin.lock() {
            *origin = None;
        }
        if let Ok(mut dragged) = self.dragged.lock() {
            *dragged = false;
        }
        if let Some(view) = view {
            view.resume_follow_after_click();
        }
    }

    /// Whether the completed press was a real drag (not a plain click).
    pub fn dragged(&self) -> bool {
        self.dragged.lock().map(|d| *d).unwrap_or(false)
    }

    /// Feed a left-button mouse event. Returns true when selection state
    /// changed and the caller should re-render.
    pub fn handle_mouse_event(
        &self,
        event: &MouseEvent,
        frame: &LayoutFrame,
        screen_rows: usize,
        screen_cols: usize,
    ) -> bool {
        let is_left = matches!(
            event.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Up(MouseButton::Left)
        );
        if !is_left {
            return false;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let (point, view) = resolve_point(
                    event.column,
                    event.row,
                    frame,
                    screen_rows,
                    screen_cols,
                    None,
                );
                let resume = view
                    .as_ref()
                    .and_then(|view| {
                        let snapshot = find_scroll_box(&frame.root, view)?
                            .scroll_snapshot
                            .as_ref()?;
                        Some(view.pause_follow_at(snapshot.scroll_top))
                    })
                    .unwrap_or(false);
                *self.resume_follow_on_click.lock().unwrap() = resume;
                *self.anchor_scroll.lock().unwrap() = view;
                *self.press_active.lock().unwrap() = true;
                *self.press_origin.lock().unwrap() = Some((event.column, event.row));
                *self.dragged.lock().unwrap() = false;
                *self.selection.lock().unwrap() = Some(Selection {
                    anchor: point.clone(),
                    focus: point,
                });
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if !*self.press_active.lock().unwrap() {
                    return false;
                }
                *self.dragged.lock().unwrap() = true;
                self.move_focus(event, frame, screen_rows, screen_cols)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if !*self.press_active.lock().unwrap() {
                    return false;
                }
                *self.press_active.lock().unwrap() = false;
                let changed = self.move_focus(event, frame, screen_rows, screen_cols);
                if self
                    .press_origin
                    .lock()
                    .unwrap()
                    .take()
                    .is_some_and(|origin| origin != (event.column, event.row))
                {
                    // Fast/coalesced input may omit Drag. Compare screen cells,
                    // not document coordinates changed by scrolling/reflow.
                    *self.dragged.lock().unwrap() = true;
                }
                if !*self.dragged.lock().unwrap() {
                    // A plain click is not a selection. Restore follow only
                    // when no explicit scrolling moved the paused viewport.
                    let resume = *self.resume_follow_on_click.lock().unwrap();
                    let view = self.anchor_scroll_view();
                    self.clear();
                    if resume {
                        if let Some(view) = view {
                            view.resume_follow_after_click();
                        }
                    }
                    return true;
                }
                *self.resume_follow_on_click.lock().unwrap() = false;
                changed
            }
            _ => false,
        }
    }

    /// Move the selection focus to the event's position (clamped to the
    /// anchored scroll view's viewport when the drag started inside it).
    fn move_focus(
        &self,
        event: &MouseEvent,
        frame: &LayoutFrame,
        screen_rows: usize,
        screen_cols: usize,
    ) -> bool {
        let anchor_view = self.anchor_scroll.lock().unwrap().clone();
        let (point, _) = resolve_point(
            event.column,
            event.row,
            frame,
            screen_rows,
            screen_cols,
            anchor_view.as_ref(),
        );
        let mut sel = self.selection.lock().unwrap();
        match sel.as_mut() {
            Some(sel) if sel.focus != point => {
                sel.focus = point;
                true
            }
            _ => false,
        }
    }

    /// Ordered half-open (start, end) bounds. Mouse coordinates identify cells,
    /// so include the later cell in BOTH directions before converting to the
    /// exclusive end used by highlighting and extraction. None when endpoints
    /// coincide or belong to different coordinate spaces.
    pub fn bounds(&self) -> Option<(SelectionPoint, SelectionPoint)> {
        let sel = self.selection()?;
        if sel.anchor == sel.focus || sel.anchor.in_scroll != sel.focus.in_scroll {
            return None;
        }
        let anchor_before = sel.anchor.row < sel.focus.row
            || (sel.anchor.row == sel.focus.row && sel.anchor.col < sel.focus.col);
        let (start, mut end) = if anchor_before {
            (sel.anchor, sel.focus)
        } else {
            (sel.focus, sel.anchor)
        };
        end.col = end.col.saturating_add(1);
        Some((start, end))
    }
}

/// Resolve a screen cell to a selection point.
///
/// When `anchor_view` is given (drag/release), the point is clamped into that
/// view's visible viewport — a drag that runs past the transcript edge keeps
/// selecting the first/last visible content row. Without an anchor (press),
/// the view under the pointer wins; a cell outside every scroll view resolves
/// to raw screen coordinates.
fn resolve_point(
    column: u16,
    row: u16,
    frame: &LayoutFrame,
    screen_rows: usize,
    screen_cols: usize,
    anchor_view: Option<&Arc<ScrollView>>,
) -> (SelectionPoint, Option<Arc<ScrollView>>) {
    if let Some(view) = anchor_view {
        if let Some(point) = scroll_point(view, column, row, frame) {
            return (point, Some(view.clone()));
        }
    } else {
        for view in crate::layout::get_scroll_views_at(frame, column as usize, row as usize) {
            if let Some(point) = scroll_point(&view, column, row, frame) {
                return (point, Some(view));
            }
        }
    }
    (
        SelectionPoint {
            row: (row as usize).min(screen_rows.saturating_sub(1)),
            col: (column as usize).min(screen_cols.saturating_sub(1)),
            in_scroll: false,
        },
        None,
    )
}

/// Map a screen cell into `view`'s content coordinates.
fn scroll_point(
    view: &Arc<ScrollView>,
    column: u16,
    row: u16,
    frame: &LayoutFrame,
) -> Option<SelectionPoint> {
    let lbox = find_scroll_box(&frame.root, view)?;
    let (rect, clip) = (lbox.rect, lbox.clip);
    let snapshot = lbox.scroll_snapshot.as_ref()?;
    let (x, y) = (column as usize, row as usize);
    if x < rect.x || x >= rect.x + rect.width || rect.width == 0 {
        return None;
    }
    let visible_top = rect.y.max(clip.y);
    let visible_bottom = (rect.y + rect.height).min(clip.y + clip.height);
    if visible_bottom <= visible_top {
        return None;
    }
    let pointer_row = y.clamp(visible_top, visible_bottom - 1);
    let content_row = snapshot.scroll_top + pointer_row - rect.y;
    let content_height = snapshot.document.len();
    let below_document = content_row >= content_height;
    Some(SelectionPoint {
        row: content_row.min(content_height.saturating_sub(1)),
        // Viewport padding is AFTER the document, not at the last row's
        // pointer column. In particular, a reverse drag from padding at x=0
        // must still include the entire last content row.
        col: if below_document {
            snapshot.content_width
        } else {
            (x - rect.x).min(rect.width - 1)
        },
        in_scroll: true,
    })
}

/// Locate `view`'s box (rect + clip) in the frame. Layout passes clone the
/// `ScrollView` into a fresh `Arc`, so identity is the shared scroll state.
fn find_scroll_box<'a>(lbox: &'a LayoutBox, view: &Arc<ScrollView>) -> Option<&'a LayoutBox> {
    if let Some(ref sv) = lbox.scroll_view {
        if sv.same_state(view) {
            return Some(lbox);
        }
    }
    for child in &lbox.children {
        if let Some(found) = find_scroll_box(child, view) {
            return Some(found);
        }
    }
    None
}

/// Reverse-video the selected span on the composited screen lines.
///
/// Applied after overlays and cursor-marker stripping, immediately before the
/// frame is diffed and written, exactly where upstream applies `applySelection`.
pub fn apply_selection_highlight(
    lines: &mut [String],
    selection: &SelectionState,
    frame: &LayoutFrame,
) {
    let Some((start, end)) = selection.bounds() else {
        return;
    };
    let (min_row, end_row, min_col, max_col, scroll_origin) = if start.in_scroll {
        let Some(view) = selection.anchor_scroll_view() else {
            return;
        };
        let Some(lbox) = find_scroll_box(&frame.root, &view) else {
            return;
        };
        let Some(snapshot) = lbox.scroll_snapshot.as_ref() else {
            return;
        };
        let (rect, clip) = (lbox.rect, lbox.clip);
        (
            rect.y.max(clip.y),
            (rect.y + rect.height).min(clip.y + clip.height),
            rect.x.max(clip.x),
            (rect.x + rect.width).min(clip.x + clip.width),
            Some((snapshot.scroll_top, rect.y, rect.x)),
        )
    } else {
        (0, lines.len(), 0, usize::MAX, None)
    };

    for (row, line) in lines.iter_mut().enumerate() {
        if row < min_row || row >= end_row || is_image_line(line) {
            continue;
        }
        // Map visible rows into document space, rather than clamping an
        // offscreen endpoint onto the first visible row with its old column.
        let (content_row, column_offset) = scroll_origin
            .map(|(top, y, x)| (top + row - y, x))
            .unwrap_or((row, 0));
        if content_row < start.row || content_row > end.row {
            continue;
        }
        let line_width = visible_width(line);
        let mut col_start = if content_row == start.row {
            column_offset + start.col
        } else {
            min_col
        };
        let mut col_end = if content_row == end.row {
            column_offset + end.col
        } else {
            line_width
        };
        col_start = col_start.min(line_width).max(min_col);
        col_end = col_end.min(line_width).max(col_start).min(max_col);
        if col_end <= col_start {
            continue;
        }
        if let Some(highlighted) = reverse_video_span(line, col_start, col_end) {
            *line = highlighted;
        }
    }
}

/// Extract the selected text as plain lines.
///
/// Scroll-space selections slice the scroll view's cached document rows —
/// the same rows the frame was painted from — by visible column. Screen-space
/// selections slice the frame's composited lines. Slicing (not indexing
/// characters) keeps wide graphemes and skipped padding columns honest.
pub fn extract_selection_text(selection: &SelectionState, frame: &LayoutFrame) -> Option<String> {
    let (start, end) = selection.bounds()?;
    let count = end.row.saturating_sub(start.row).saturating_add(1);
    let source: Vec<String> = if start.in_scroll {
        let view = selection.anchor_scroll_view()?;
        let snapshot = find_scroll_box(&frame.root, &view)?
            .scroll_snapshot
            .as_ref()?;
        // Retain the painted document, not a mutable cache refreshed by a
        // render that the input loop has not seen yet. Copy only selected rows.
        snapshot.document.lines(start.row, count)
    } else {
        frame
            .lines
            .iter()
            .skip(start.row)
            .take(count)
            .cloned()
            .collect()
    };
    if source.is_empty() {
        return None;
    }
    let mut extracted = Vec::with_capacity(source.len());
    for (index, line) in source.iter().enumerate() {
        let row = start.row + index;
        let line_width = visible_width(line);
        let mut col_start = if row == start.row { start.col } else { 0 };
        let mut col_end = if row == end.row { end.col } else { line_width };
        col_start = col_start.min(line_width);
        col_end = col_end.min(line_width).max(col_start);
        let span = if col_end > col_start {
            line_diff::snap_column_span(line, col_start, col_end)
                .and_then(|(start, end)| line_diff::slice(line, start, end - start))
                .map(|text| strip_ansi(&text))
                .unwrap_or_default()
        } else {
            String::new()
        };
        extracted.push(span.trim_end().to_string());
    }
    let text = extracted.join("\n");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Wrap a column span in reverse video, re-asserting it after every SGR run
/// inside the span so an embedded reset (`\x1b[0m`) cannot cancel the
/// highlight (upstream `applySelectionHighlight`).
fn reverse_video_span(line: &str, start_col: usize, end_col: usize) -> Option<String> {
    line_diff::highlight_span(line, start_col, end_col)
}

fn is_image_line(line: &str) -> bool {
    line.contains("\x1b_G") || line.contains("\x1b]1337;File=")
}

impl SelectionState {
    /// The scroll view this selection is anchored in, if any. Public for
    /// extraction/highlight mapping within the crate.
    pub(crate) fn anchor_scroll_view(&self) -> Option<Arc<ScrollView>> {
        self.anchor_scroll.lock().ok().and_then(|v| v.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::render_layout_frame;
    use crate::scroll_view::{FollowMode, ScrollViewOptions};
    use crate::{Container, Text};

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// A transcript document long enough to scroll: 30 numbered rows in a
    /// 12-row scroll viewport.
    fn scrolled_frame() -> (Arc<ScrollView>, LayoutFrame) {
        let doc = Container::new();
        for i in 0..30 {
            doc.add_child(Arc::new(Text::new(format!("line {:02} content", i), 0, 0)));
        }
        let view = Arc::new(ScrollView::new(
            Arc::new(doc) as Arc<dyn crate::component::Component>,
            ScrollViewOptions {
                follow: FollowMode::End,
                primary: true,
                ..Default::default()
            },
        ));
        let frame = render_layout_frame(view.clone(), 40, 12);
        (view, frame)
    }

    fn drag_from_to(state: &SelectionState, frame: &LayoutFrame, from: (u16, u16), to: (u16, u16)) {
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), from.0, from.1),
            frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1),
            frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1),
            frame,
            12,
            40,
        );
    }

    #[test]
    fn middle_selection_does_not_jump_to_row_start_when_output_grows() {
        let text = Arc::new(Text::new(
            (0..30)
                .map(|i| format!("line {i:02} content"))
                .collect::<Vec<_>>()
                .join("\n"),
            0,
            0,
        ));
        let view = Arc::new(ScrollView::new(
            text.clone(),
            ScrollViewOptions {
                follow: FollowMode::End,
                primary: true,
                ..Default::default()
            },
        ));
        let frame = render_layout_frame(view.clone(), 40, 12);
        let state = SelectionState::new();
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 5, 2),
            &frame,
            12,
            40,
        );
        // Streaming or a dock resize used to move the viewport between Down
        // and Drag, turning a middle-of-row selection into complete rows.
        text.set_text(
            (0..45)
                .map(|i| format!("line {i:02} content"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let frame = render_layout_frame(view.clone(), 40, 12);
        state.handle_mouse_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 6, 2),
            &frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 6, 2),
            &frame,
            12,
            40,
        );
        assert_eq!(
            extract_selection_text(&state, &frame).as_deref(),
            Some("20")
        );
        let mut lines = frame.lines.clone();
        apply_selection_highlight(&mut lines, &state, &frame);
        assert!(
            lines[2].contains("line \x1b[7m20\x1b[27m content"),
            "{:?}",
            lines[2]
        );
        assert!(!view.is_following_end());
    }

    #[test]
    fn selection_resolves_against_painted_frame_not_live_scroll_or_cache() {
        let text = Arc::new(Text::new(
            (0..30)
                .map(|i| format!("line {i:02} content"))
                .collect::<Vec<_>>()
                .join("\n"),
            0,
            0,
        ));
        let view = Arc::new(ScrollView::new(
            text.clone(),
            ScrollViewOptions {
                follow: FollowMode::End,
                primary: true,
                ..Default::default()
            },
        ));
        let painted = render_layout_frame(view.clone(), 40, 12);
        // A render is in flight, but the input loop still sees the old frame.
        text.set_text(
            (0..45)
                .map(|i| format!("line {i:02} REPLACED"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let _in_flight = render_layout_frame(view.clone(), 40, 12);
        let state = SelectionState::new();
        drag_from_to(&state, &painted, (5, 2), (12, 2));
        assert_eq!(
            extract_selection_text(&state, &painted).as_deref(),
            Some("20 conte")
        );
        let mut lines = painted.lines.clone();
        apply_selection_highlight(&mut lines, &state, &painted);
        assert!(
            lines[2].contains("line \x1b[7m20 conte\x1b[27mnt"),
            "{:?}",
            lines[2]
        );
    }

    #[test]
    fn plain_click_collapses_and_never_selects() {
        let state = SelectionState::new();
        let (view, frame) = scrolled_frame();
        assert!(view.is_following_end());
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 2, 2),
            &frame,
            12,
            40,
        );
        assert!(!view.is_following_end());
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 2, 2),
            &frame,
            12,
            40,
        );
        assert!(!state.dragged());
        assert!(state.bounds().is_none());
        assert!(extract_selection_text(&state, &frame).is_none());
        assert!(
            view.is_following_end(),
            "a plain click must not disable follow"
        );
    }

    #[test]
    fn plain_click_restores_follow_after_layout_clamp_but_respects_navigation() {
        for navigate in [false, true] {
            let state = SelectionState::new();
            let (view, frame) = scrolled_frame();
            state.handle_mouse_event(
                &mouse(MouseEventKind::Down(MouseButton::Left), 5, 2),
                &frame,
                12,
                40,
            );
            let frame = render_layout_frame(view.clone(), 40, 20);
            assert_eq!(
                view.scroll_top(),
                10,
                "larger viewport clamps the paused top"
            );
            if navigate {
                view.scroll_by(-1);
            }
            state.handle_mouse_event(
                &mouse(MouseEventKind::Up(MouseButton::Left), 5, 2),
                &frame,
                20,
                40,
            );
            assert!(state.bounds().is_none());
            assert_eq!(view.is_following_end(), !navigate);
        }
    }

    #[test]
    fn drag_extracts_the_highlighted_rows_by_visible_column() {
        let state = SelectionState::new();
        let (_view, frame) = scrolled_frame();
        // The scroll view fills the whole frame; content is 30 rows in a
        // 12-row viewport following the end, so screen row 0 = content row 18.
        drag_from_to(&state, &frame, (5, 0), (12, 1));
        assert!(state.dragged());
        let sel = state.bounds().expect("drag produced a selection");
        assert!(sel.0.in_scroll && sel.1.in_scroll);
        assert_eq!(sel.0.row, 18);
        assert_eq!(sel.1.row, 19);
        // Columns 5..=12: "18 content" / "line 19 conte".
        let text = extract_selection_text(&state, &frame).expect("non-empty text");
        assert_eq!(text, "18 content\nline 19 conte", "got: {text:?}");
    }

    #[test]
    fn drag_keeps_selecting_the_same_content_when_the_view_scrolls() {
        let state = SelectionState::new();
        let (view, frame) = scrolled_frame();
        // Anchor near the top of the viewport.
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &frame,
            12,
            40,
        );
        // The view scrolls up by 3 rows mid-drag…
        view.scroll_by(-3);
        // …the renderer lays out the new position…
        let frame = render_layout_frame(view.clone(), 40, 12);
        // …and the drag continues at the same screen cell.
        state.handle_mouse_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 10, 1),
            &frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 10, 1),
            &frame,
            12,
            40,
        );
        let sel = state.bounds().expect("selection survived the scroll");
        // Anchor resolved against the pre-scroll frame (content row 18); the
        // release resolves against the post-scroll frame where screen row 1
        // is content row 16. Selection covers content rows 16..=18.
        assert_eq!(sel.0.row, 16);
        assert_eq!(sel.1.row, 18);
    }

    #[test]
    fn a_drag_out_of_the_transcript_clamps_to_the_last_visible_content_row() {
        let state = SelectionState::new();
        let (_view, frame) = scrolled_frame();
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &frame,
            12,
            40,
        );
        // Drag past the bottom of the scroll viewport (row 11 is the last).
        state.handle_mouse_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 5, 11),
            &frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 5, 11),
            &frame,
            12,
            40,
        );
        let sel = state.bounds().expect("clamped selection");
        assert_eq!(sel.1.row, 29, "clamped to the last content row");
    }

    #[test]
    fn selection_outside_any_scroll_view_uses_screen_coordinates() {
        let state = SelectionState::new();
        // A plain container root: no scroll view anywhere.
        let doc = Container::new();
        doc.add_child(Arc::new(Text::new("alpha beta", 0, 0)));
        doc.add_child(Arc::new(Text::new("gamma delta", 0, 0)));
        let frame = render_layout_frame(
            Arc::new(doc) as Arc<dyn crate::component::Component>,
            40,
            12,
        );
        drag_from_to(&state, &frame, (0, 0), (5, 1));
        let sel = state.bounds().expect("screen-space selection");
        assert!(!sel.0.in_scroll && !sel.1.in_scroll);
        let text = extract_selection_text(&state, &frame).expect("text");
        // Columns 0..=5 of row 0, 0..=5 of row 1 (trailing blanks trimmed).
        assert_eq!(text, "alpha beta\ngamma", "got: {text:?}");
    }

    #[test]
    fn scrollbar_width_is_only_subtracted_once_when_extracting() {
        let view = Arc::new(ScrollView::new(
            Arc::new(Text::new("hello world", 0, 0)),
            ScrollViewOptions {
                scrollbar: crate::scroll_view::ScrollbarMode::Always,
                ..Default::default()
            },
        ));
        let frame = render_layout_frame(view, 12, 12);
        let state = SelectionState::new();
        drag_from_to(&state, &frame, (0, 0), (5, 0));
        assert_eq!(
            extract_selection_text(&state, &frame).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn multiline_highlight_keeps_both_end_rows_in_place_in_either_direction() {
        let doc = Container::new();
        doc.add_child(Arc::new(Text::new("hello world", 0, 0)));
        doc.add_child(Arc::new(Text::new("middle row", 0, 0)));
        doc.add_child(Arc::new(Text::new("last tail", 0, 0)));
        let frame = render_layout_frame(Arc::new(doc), 40, 12);
        for (from, to) in [((6, 0), (3, 2)), ((3, 2), (6, 0))] {
            let state = SelectionState::new();
            drag_from_to(&state, &frame, from, to);
            assert_eq!(
                extract_selection_text(&state, &frame).as_deref(),
                Some("world\nmiddle row\nlast")
            );
            let mut lines = frame.lines.clone();
            apply_selection_highlight(&mut lines, &state, &frame);
            for (painted, original) in lines.iter().zip(&frame.lines) {
                assert_eq!(strip_ansi(painted), strip_ansi(original));
            }
            assert!(lines[0].contains("hello \x1b[7mworld"));
            assert!(lines[2].contains("\x1b[7mlast\x1b[27m tail"));
        }
    }

    #[test]
    fn partial_highlight_preserves_the_entire_row() {
        for line in [
            "hello world test",
            "\x1b[31mhello\x1b[0m world test",
            "\x1b]8;;https://example.com\x07hello world\x1b]8;;\x07 test",
            "a中文👩‍💻z",
        ] {
            let highlighted = reverse_video_span(line, 2, 8).unwrap();
            assert_eq!(strip_ansi(&highlighted), strip_ansi(line));
            assert_eq!(visible_width(&highlighted), visible_width(line));
        }
        let highlighted = reverse_video_span("hello world test", 6, 11).unwrap();
        assert_eq!(highlighted, "hello \x1b[7mworld\x1b[27m test");
    }

    #[test]
    fn wide_grapheme_selection_copies_the_whole_highlighted_grapheme() {
        let doc = Arc::new(Text::new("a中👩‍💻z", 0, 0));
        let frame = render_layout_frame(doc, 40, 12);
        for (from, to, expected) in [(2, 4, "中👩‍💻"), (4, 2, "中👩‍💻")] {
            let state = SelectionState::new();
            drag_from_to(&state, &frame, (from, 0), (to, 0));
            assert_eq!(
                extract_selection_text(&state, &frame).as_deref(),
                Some(expected)
            );
            let mut lines = frame.lines.clone();
            apply_selection_highlight(&mut lines, &state, &frame);
            assert_eq!(strip_ansi(&lines[0]), strip_ansi(&frame.lines[0]));
        }
    }

    #[test]
    fn clearing_cancels_an_in_progress_drag() {
        let state = SelectionState::new();
        let (_, frame) = scrolled_frame();
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 5, 1),
            &frame,
            12,
            40,
        );
        state.clear();
        assert!(!state.dragged());
        assert!(!state.handle_mouse_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 8, 2),
            &frame,
            12,
            40
        ));
        assert!(!state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 8, 2),
            &frame,
            12,
            40
        ));
        assert!(state.bounds().is_none());
    }

    #[test]
    fn scrolled_off_endpoints_do_not_select_unrelated_visible_rows() {
        let state = SelectionState::new();
        let (view, _) = scrolled_frame();
        view.scroll_by(-5);
        let frame = render_layout_frame(view.clone(), 40, 12);
        drag_from_to(&state, &frame, (5, 0), (10, 2));
        view.scroll_by(1); // Start row 13 is now above the viewport.
        let frame = render_layout_frame(view.clone(), 40, 12);
        let mut lines = frame.lines.clone();
        apply_selection_highlight(&mut lines, &state, &frame);
        assert!(lines[0].contains("\x1b[7mline 14"), "{:?}", lines[0]);
        assert_eq!(strip_ansi(&lines[0]), strip_ansi(&frame.lines[0]));

        view.scroll_by(-10); // Entire selection is now below the viewport.
        let frame = render_layout_frame(view, 40, 4);
        let mut lines = frame.lines.clone();
        apply_selection_highlight(&mut lines, &state, &frame);
        assert_eq!(lines, frame.lines);
    }

    #[test]
    fn dragging_between_character_cells_includes_both_ends_in_either_direction() {
        for text in [
            "ABCDE",
            "0123456789012345678901234567890123456789",
            "你好世界",
        ] {
            let width = visible_width(text);
            let frame = render_layout_frame(Arc::new(Text::new(text, 0, 0)), width, 12);
            for (from, to) in [
                ((width as u16 - 1, 0), (0, 0)),
                ((0, 0), (width as u16 - 1, 0)),
            ] {
                let state = SelectionState::new();
                drag_from_to(&state, &frame, from, to);
                assert_eq!(
                    extract_selection_text(&state, &frame).as_deref(),
                    Some(text)
                );
                let mut lines = frame.lines.clone();
                apply_selection_highlight(&mut lines, &state, &frame);
                assert_eq!(strip_ansi(&lines[0]), text);
                assert!(lines[0].contains("\x1b[7m"));
            }
        }
    }

    #[test]
    fn reverse_drag_from_padding_below_document_includes_the_last_row() {
        let doc = Container::new();
        doc.add_child(Arc::new(Text::new("first", 0, 0)));
        doc.add_child(Arc::new(Text::new("last", 0, 0)));
        let view = Arc::new(ScrollView::new(Arc::new(doc), ScrollViewOptions::default()));
        let frame = render_layout_frame(view, 40, 12);
        for col in [0, 2, 39] {
            let state = SelectionState::new();
            drag_from_to(&state, &frame, (col, 8), (0, 0));
            assert_eq!(
                extract_selection_text(&state, &frame).as_deref(),
                Some("first\nlast")
            );
            let mut lines = frame.lines.clone();
            apply_selection_highlight(&mut lines, &state, &frame);
            assert!(lines[1].contains("\x1b[7mlast"));
        }
    }

    #[test]
    fn stationary_click_does_not_become_a_drag_when_document_scrolls() {
        let (view, frame) = scrolled_frame();
        let state = SelectionState::new();
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 2, 2),
            &frame,
            12,
            40,
        );
        view.scroll_by(-1);
        let frame = render_layout_frame(view, 40, 12);
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 2, 2),
            &frame,
            12,
            40,
        );
        assert!(!state.dragged());
        assert!(extract_selection_text(&state, &frame).is_none());
    }

    #[test]
    fn release_at_a_different_cell_completes_a_drag_even_without_motion_events() {
        let frame = render_layout_frame(Arc::new(Text::new("ABCDE", 0, 0)), 40, 12);
        let state = SelectionState::new();
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 4, 0),
            &frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 0, 0),
            &frame,
            12,
            40,
        );
        assert!(state.dragged());
        assert_eq!(
            extract_selection_text(&state, &frame).as_deref(),
            Some("ABCDE")
        );
    }

    #[test]
    fn reverse_video_wraps_the_span_and_survives_embedded_resets() {
        let line = "\x1b[31mred\x1b[0m plain";
        let highlighted = reverse_video_span(line, 0, visible_width(line)).expect("sliceable line");
        assert!(highlighted.starts_with("\x1b[7m"));
        assert!(highlighted.ends_with("\x1b[27m"));
        // The embedded reset must be followed by a re-asserted inverse.
        let reset = highlighted.find("\x1b[0m").expect("reset preserved");
        assert!(
            highlighted[reset + 4..].starts_with("\x1b[7m"),
            "inverse re-asserted after the reset: {highlighted:?}"
        );
    }

    #[test]
    fn image_lines_are_never_highlighted() {
        let state = SelectionState::new();
        let doc = Container::new();
        doc.add_child(Arc::new(Text::new("above", 0, 0)));
        doc.add_child(Arc::new(Text::new("\x1b_Gd=7;abc\x1b\\caption", 0, 0)));
        let frame = render_layout_frame(
            Arc::new(doc) as Arc<dyn crate::component::Component>,
            40,
            12,
        );
        drag_from_to(&state, &frame, (0, 0), (10, 2));
        let mut lines = frame.lines.clone();
        apply_selection_highlight(&mut lines, &state, &frame);
        // The kitty line must come through untouched.
        assert!(lines[1].contains("\x1b_G") && !lines[1].contains("\x1b[7m"));
        // The plain line above IS highlighted.
        assert!(lines[0].contains("\x1b[7m"));
    }
}
