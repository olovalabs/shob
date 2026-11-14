use gpui::{Context, Point, Window};

use crate::input::{
    InputState, MoveDown, MoveEnd, MoveHome, MoveLeft, MovePageDown, MovePageUp, MoveRight,
    MoveToEnd, MoveToNextWord, MoveToPreviousWord, MoveToStart, MoveUp, RopeExt as _,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveDirection {
    Up,
    Down,
}

impl InputState {

    pub(super) fn update_preferred_column(&mut self) {
        let Some(last_layout) = &self.last_layout else {
            self.preferred_column = None;
            return;
        };

        let point = self.text.offset_to_point(self.cursor());
        let row = point.row.saturating_sub(last_layout.visible_range.start);
        let Some(line) = last_layout.lines.get(row) else {
            self.preferred_column = None;
            return;
        };

        let Some(pos) = line.position_for_index(point.column, last_layout.line_height) else {
            self.preferred_column = None;
            return;
        };

        self.preferred_column = Some((pos.x, point.column));
    }

    pub(crate) fn move_to(
        &mut self,
        offset: usize,
        direction: Option<MoveDirection>,
        cx: &mut Context<Self>,
    ) {
        let offset = offset.clamp(0, self.text.len());
        self.selected_range = (offset..offset).into();
        self.scroll_to(offset, direction, cx);
        self.pause_blink_cursor(cx);
        self.update_preferred_column();
        self.hide_context_menu(cx);
        self.clear_inline_completion(cx);
        self.diagnostic_popover = None;
        cx.notify()
    }

    pub(super) fn move_vertical(
        &mut self,
        move_lines: isize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode.is_single_line() {
            return;
        }
        let Some(last_layout) = &self.last_layout else {
            return;
        };

        let offset = self.cursor();
        let was_preferred_column = self.preferred_column;

        let mut display_point = self.text_wrapper.offset_to_display_point(offset);
        display_point.row = display_point.row.saturating_add_signed(move_lines);
        display_point.column = 0;
        let mut new_offset = self.text_wrapper.display_point_to_offset(display_point);

        if let Some((preferred_x, column)) = was_preferred_column {

            let mut next_display_point = self.text_wrapper.offset_to_display_point(new_offset);
            next_display_point.column = 0;
            let next_point = self.text_wrapper.display_point_to_point(next_display_point);
            let line_start_offset = self.text.line_start_offset(next_point.row);

            if let Some(line) = last_layout.line(next_point.row) {
                if let Some(x) = line.closest_index_for_position(
                    Point {
                        x: preferred_x,
                        y: next_display_point.local_row * last_layout.line_height,
                    },
                    last_layout.line_height,
                ) {
                    new_offset = line_start_offset + x;
                }
            } else {

                let max_line_len = self.text.slice_line(next_point.row).len();
                new_offset = line_start_offset + column.min(max_line_len);
            }
        }

        self.pause_blink_cursor(cx);
        let direction = if move_lines < 0 {
            MoveDirection::Up
        } else {
            MoveDirection::Down
        };
        self.move_to(new_offset, Some(direction), cx);

        self.preferred_column = was_preferred_column;
        cx.notify();
    }

    pub(super) fn left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.pause_blink_cursor(cx);
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor()), None, cx);
        } else {
            self.move_to(self.selected_range.start, None, cx)
        }
    }

    pub(super) fn right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.pause_blink_cursor(cx);
        if self.selected_range.is_empty() {
            // At the end of the text there is nowhere to move: let the
            // embedder use Right to accept an inline completion.
            if self.selected_range.end >= self.text.len() {
                cx.propagate();
                return;
            }
            self.move_to(self.next_boundary(self.selected_range.end), None, cx);
        } else {
            self.move_to(self.selected_range.end, None, cx)
        }
    }

    pub(super) fn up(&mut self, action: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.handle_action_for_context_menu(Box::new(action.clone()), window, cx) {
            return;
        }

        if self.mode.is_single_line() {
            return;
        }

        // At the top of the buffer there is nowhere to move: hand the key to
        // the embedder, which may use it for shell/history recall.
        if self.cursor_position().line == 0 {
            self.pause_blink_cursor(cx);
            cx.propagate();
            return;
        }

        if !self.selected_range.is_empty() {
            self.move_to(
                self.previous_boundary(self.selected_range.start.saturating_sub(1)),
                Some(MoveDirection::Up),
                cx,
            );
        }
        self.pause_blink_cursor(cx);
        self.move_vertical(-1, window, cx);
    }

    pub(super) fn down(&mut self, action: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        if self.handle_action_for_context_menu(Box::new(action.clone()), window, cx) {
            return;
        }

        if self.mode.is_single_line() {
            return;
        }

        // Same hand-off at the bottom of the buffer.
        let last_line = self.text.lines_len().saturating_sub(1);
        if self.cursor_position().line as usize >= last_line {
            self.pause_blink_cursor(cx);
            cx.propagate();
            return;
        }

        if !self.selected_range.is_empty() {
            self.move_to(
                self.next_boundary(self.selected_range.end.saturating_sub(1)),
                Some(MoveDirection::Down),
                cx,
            );
        }

        self.pause_blink_cursor(cx);
        self.move_vertical(1, window, cx);
    }

    pub(super) fn page_up(&mut self, _: &MovePageUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode.is_single_line() {
            return;
        }

        let Some(last_layout) = &self.last_layout else {
            return;
        };

        let display_lines = (self.input_bounds.size.height / last_layout.line_height) as isize;
        self.move_vertical(-display_lines, window, cx);
    }

    pub(super) fn page_down(
        &mut self,
        _: &MovePageDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode.is_single_line() {
            return;
        }

        let Some(last_layout) = &self.last_layout else {
            return;
        };

        let display_lines = (self.input_bounds.size.height / last_layout.line_height) as isize;
        self.move_vertical(display_lines, window, cx);
    }

    pub(super) fn home(&mut self, _: &MoveHome, _: &mut Window, cx: &mut Context<Self>) {
        self.pause_blink_cursor(cx);
        let offset = self.start_of_line();
        self.move_to(offset, Some(MoveDirection::Up), cx);
    }

    pub(super) fn end(&mut self, _: &MoveEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.pause_blink_cursor(cx);
        let offset = self.end_of_line();
        self.move_to(offset, Some(MoveDirection::Down), cx);
    }

    pub fn move_to_start_of_line(&mut self, cx: &mut Context<Self>) {
        self.pause_blink_cursor(cx);
        let offset = self.start_of_line();
        self.move_to(offset, Some(MoveDirection::Up), cx);
    }

    pub fn move_to_end_of_line(&mut self, cx: &mut Context<Self>) {
        self.pause_blink_cursor(cx);
        let offset = self.end_of_line();
        self.move_to(offset, Some(MoveDirection::Down), cx);
    }

    pub(super) fn move_to_start(
        &mut self,
        _: &MoveToStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(0, None, cx);
    }

    pub(super) fn move_to_end(&mut self, _: &MoveToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.text.len(), None, cx);
    }

    pub(super) fn move_to_previous_word(
        &mut self,
        _: &MoveToPreviousWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.previous_start_of_word();
        self.move_to(offset, None, cx);
    }

    pub(super) fn move_to_next_word(
        &mut self,
        _: &MoveToNextWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.next_end_of_word();
        self.move_to(offset, None, cx);
    }
}
