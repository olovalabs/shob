use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::term::TermMode;
use gpui::{MouseButton, Pixels, Point};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionType {
    Simple,

    Word,

    Line,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub start: AlacPoint,

    pub end: AlacPoint,

    pub selection_type: SelectionType,
}

impl Selection {
    pub fn new(start: AlacPoint, end: AlacPoint, selection_type: SelectionType) -> Self {
        Self {
            start,
            end,
            selection_type,
        }
    }

    pub fn contains(&self, point: AlacPoint) -> bool {
        let (start, end) = if self.start < self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        };

        point >= start && point <= end
    }
}

pub fn pixel_to_cell(
    position: Point<Pixels>,
    origin: Point<Pixels>,
    cell_width: Pixels,
    cell_height: Pixels,
) -> AlacPoint {
    let col = ((position.x - origin.x) / cell_width).floor();
    let col = col.max(0.0) as usize;

    let row = ((position.y - origin.y) / cell_height).floor();
    let row = row.max(0.0) as i32;

    AlacPoint::new(Line(row), Column(col))
}

pub fn selection_type_from_clicks(click_count: usize) -> SelectionType {
    match click_count {
        1 => SelectionType::Simple,
        2 => SelectionType::Word,
        _ => SelectionType::Line,
    }
}

fn sgr_mouse_report(point: AlacPoint, button: u8, pressed: bool) -> Vec<u8> {
    let action = if pressed { 'M' } else { 'm' };
    let col = point.column.0 + 1;
    let row = point.line.0 + 1;
    format!("\x1b[<{};{};{}{}", button, col, row, action).into_bytes()
}

fn normal_mouse_report(point: AlacPoint, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let max_point = if utf8 { 2015 } else { 223 };

    let col = point.column.0;
    let row = point.line.0;
    if row < 0 || (row as usize) >= max_point || col >= max_point {
        return None;
    }

    let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

    let mouse_pos_encode = |pos: usize| -> Vec<u8> {
        let pos = 32 + 1 + pos;
        let first = 0xC0 + pos / 64;
        let second = 0x80 + (pos & 63);
        vec![first as u8, second as u8]
    };

    if utf8 && col >= 95 {
        msg.append(&mut mouse_pos_encode(col));
    } else {
        msg.push(32 + 1 + col as u8);
    }

    if utf8 && row >= 95 {
        msg.append(&mut mouse_pos_encode(row as usize));
    } else {
        msg.push(32 + 1 + row as u8);
    }

    Some(msg)
}

fn format_mouse_report(
    point: AlacPoint,
    button_value: u8,
    pressed: bool,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if point.line.0 < 0 {
        return None;
    }

    if mode.contains(TermMode::SGR_MOUSE) {
        Some(sgr_mouse_report(point, button_value, pressed))
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        if pressed {
            normal_mouse_report(point, button_value, true)
        } else {
            let modifiers = button_value & (4 | 8 | 16);
            normal_mouse_report(point, 3 | modifiers, true)
        }
    } else {
        if pressed {
            normal_mouse_report(point, button_value, false)
        } else {
            let modifiers = button_value & (4 | 8 | 16);
            normal_mouse_report(point, 3 | modifiers, false)
        }
    }
}

pub fn mouse_button_report(
    button: MouseButton,
    pressed: bool,
    point: AlacPoint,
    modifiers: u8,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if !mode
        .intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
    {
        return None;
    }

    let button_code = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        _ => return None,
    };

    let button_value = button_code | modifiers;
    format_mouse_report(point, button_value, pressed, mode)
}

pub fn mouse_motion_report(
    point: AlacPoint,
    button: Option<MouseButton>,
    modifiers: u8,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let motion_enabled = mode.contains(TermMode::MOUSE_MOTION);
    let drag_enabled = mode.contains(TermMode::MOUSE_DRAG) && button.is_some();

    if !motion_enabled && !drag_enabled {
        return None;
    }

    let button_code = match button {
        Some(MouseButton::Left) => 32,
        Some(MouseButton::Middle) => 33,
        Some(MouseButton::Right) => 34,
        _ => 35,
    };

    let button_value = button_code | modifiers;
    format_mouse_report(point, button_value, true, mode)
}

pub fn scroll_report(
    delta: i32,
    point: AlacPoint,
    modifiers: u8,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if delta == 0 {
        return None;
    }

    let shift_held = (modifiers & 4) != 0;

    if !shift_held
        && mode.intersects(
            TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG,
        )
    {
        let button_code = if delta > 0 { 64 } else { 65 };
        let button_value = button_code | modifiers;

        if let Some(single_report) = format_mouse_report(point, button_value, true, mode) {
            let count = delta.unsigned_abs().min(10) as usize;
            let mut result = Vec::with_capacity(single_report.len() * count);
            for _ in 0..count {
                result.extend_from_slice(&single_report);
            }
            return Some(result);
        }
    }

    if !shift_held && (mode.contains(TermMode::ALT_SCREEN) || mode.contains(TermMode::APP_CURSOR)) {
        return Some(scroll_to_arrow_keys(delta, mode));
    }

    None
}

fn scroll_to_arrow_keys(delta: i32, mode: TermMode) -> Vec<u8> {
    let count = delta.unsigned_abs().min(10) as usize;

    let arrow_seq = if delta > 0 {
        if mode.contains(TermMode::APP_CURSOR) {
            b"\x1bOA"
        } else {
            b"\x1b[A"
        }
    } else {
        if mode.contains(TermMode::APP_CURSOR) {
            b"\x1bOB"
        } else {
            b"\x1b[B"
        }
    };

    let mut result = Vec::with_capacity(arrow_seq.len() * count);
    for _ in 0..count {
        result.extend_from_slice(arrow_seq);
    }
    result
}

pub fn encode_modifiers(shift: bool, alt: bool, control: bool) -> u8 {
    let mut modifiers = 0;
    if shift {
        modifiers |= 4;
    }
    if alt {
        modifiers |= 8;
    }
    if control {
        modifiers |= 16;
    }
    modifiers
}

pub fn pixels_to_scroll_lines(pixel_delta: Pixels, cell_height: Pixels) -> i32 {
    let lines = (pixel_delta / cell_height).round();

    lines.clamp(-10.0, 10.0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};

    #[test]
    fn test_pixel_to_cell() {
        let position = point(px(100.0), px(50.0));
        let origin = point(px(10.0), px(10.0));
        let cell_width = px(10.0);
        let cell_height = px(20.0);

        let point = pixel_to_cell(position, origin, cell_width, cell_height);
        assert_eq!(point.column.0, 9);
        assert_eq!(point.line.0, 2);
    }

    #[test]
    fn test_pixel_to_cell_at_origin() {
        let position = point(px(10.0), px(10.0));
        let origin = point(px(10.0), px(10.0));
        let cell_width = px(10.0);
        let cell_height = px(20.0);

        let point = pixel_to_cell(position, origin, cell_width, cell_height);
        assert_eq!(point.column.0, 0);
        assert_eq!(point.line.0, 0);
    }

    #[test]
    fn test_pixel_to_cell_negative_coordinates() {
        let position = point(px(5.0), px(5.0));
        let origin = point(px(10.0), px(10.0));
        let cell_width = px(10.0);
        let cell_height = px(20.0);

        let point = pixel_to_cell(position, origin, cell_width, cell_height);
        assert_eq!(point.column.0, 0);
        assert_eq!(point.line.0, 0);
    }

    #[test]
    fn test_selection_type_from_clicks() {
        assert_eq!(selection_type_from_clicks(1), SelectionType::Simple);
        assert_eq!(selection_type_from_clicks(2), SelectionType::Word);
        assert_eq!(selection_type_from_clicks(3), SelectionType::Line);
        assert_eq!(selection_type_from_clicks(4), SelectionType::Line);
        assert_eq!(selection_type_from_clicks(10), SelectionType::Line);
    }

    #[test]
    fn test_selection_contains() {
        let selection = Selection::new(
            AlacPoint::new(Line(5), Column(10)),
            AlacPoint::new(Line(7), Column(20)),
            SelectionType::Simple,
        );

        assert!(selection.contains(AlacPoint::new(Line(6), Column(15))));

        assert!(selection.contains(AlacPoint::new(Line(5), Column(10))));
        assert!(selection.contains(AlacPoint::new(Line(7), Column(20))));

        assert!(!selection.contains(AlacPoint::new(Line(4), Column(15))));
        assert!(!selection.contains(AlacPoint::new(Line(8), Column(15))));
    }

    #[test]
    fn test_selection_contains_reverse() {
        let selection = Selection::new(
            AlacPoint::new(Line(7), Column(20)),
            AlacPoint::new(Line(5), Column(10)),
            SelectionType::Simple,
        );

        assert!(selection.contains(AlacPoint::new(Line(6), Column(15))));
        assert!(selection.contains(AlacPoint::new(Line(5), Column(10))));
        assert!(selection.contains(AlacPoint::new(Line(7), Column(20))));
    }

    #[test]
    fn test_mouse_button_report_left_click() {
        let point = AlacPoint::new(Line(5), Column(10));
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;

        let bytes = mouse_button_report(MouseButton::Left, true, point, 0, mode);
        assert!(bytes.is_some());

        let sequence = String::from_utf8(bytes.unwrap()).unwrap();

        assert_eq!(sequence, "\x1b[<0;11;6M");
    }

    #[test]
    fn test_mouse_button_report_normal_x10() {
        let point = AlacPoint::new(Line(5), Column(10));
        let mode = TermMode::MOUSE_REPORT_CLICK;

        let bytes = mouse_button_report(MouseButton::Left, true, point, 0, mode);
        assert!(bytes.is_some());

        assert_eq!(bytes.unwrap(), vec![0x1b, b'[', b'M', 32, 43, 38]);
    }

    #[test]
    fn test_mouse_button_report_right_release() {
        let point = AlacPoint::new(Line(0), Column(0));
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;

        let bytes = mouse_button_report(MouseButton::Right, false, point, 0, mode);
        assert!(bytes.is_some());

        let sequence = String::from_utf8(bytes.unwrap()).unwrap();

        assert_eq!(sequence, "\x1b[<2;1;1m");
    }

    #[test]
    fn test_mouse_button_report_with_modifiers() {
        let point = AlacPoint::new(Line(0), Column(0));
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let modifiers = encode_modifiers(true, true, true);

        let bytes = mouse_button_report(MouseButton::Left, true, point, modifiers, mode);
        assert!(bytes.is_some());

        let sequence = String::from_utf8(bytes.unwrap()).unwrap();

        assert_eq!(sequence, "\x1b[<28;1;1M");
    }

    #[test]
    fn test_mouse_button_report_disabled() {
        let point = AlacPoint::new(Line(0), Column(0));
        let mode = TermMode::empty();

        let bytes = mouse_button_report(MouseButton::Left, true, point, 0, mode);
        assert!(bytes.is_none());
    }

    #[test]
    fn test_scroll_report_mouse_mode() {
        let point = AlacPoint::new(Line(5), Column(10));
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;

        let bytes = scroll_report(1, point, 0, mode);
        assert!(bytes.is_some());
        let sequence = String::from_utf8(bytes.unwrap()).unwrap();

        assert_eq!(sequence, "\x1b[<64;11;6M");

        let bytes = scroll_report(3, point, 0, mode);
        assert!(bytes.is_some());
        let sequence = String::from_utf8(bytes.unwrap()).unwrap();
        assert_eq!(sequence, "\x1b[<64;11;6M\x1b[<64;11;6M\x1b[<64;11;6M");

        let bytes = scroll_report(-2, point, 0, mode);
        assert!(bytes.is_some());
        let sequence = String::from_utf8(bytes.unwrap()).unwrap();

        assert_eq!(sequence, "\x1b[<65;11;6M\x1b[<65;11;6M");
    }

    #[test]
    fn test_scroll_report_normal_x10_mouse_mode() {
        let point = AlacPoint::new(Line(5), Column(10));
        let mode = TermMode::MOUSE_REPORT_CLICK;

        let bytes = scroll_report(1, point, 0, mode);
        assert!(bytes.is_some());

        assert_eq!(bytes.unwrap(), vec![0x1b, b'[', b'M', 96, 43, 38]);
    }

    #[test]
    fn test_scroll_report_shift_bypass() {
        let point = AlacPoint::new(Line(0), Column(0));
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let shift_mod = 4;

        let bytes = scroll_report(3, point, shift_mod, mode);
        assert!(bytes.is_none());
    }

    #[test]
    fn test_scroll_report_alternate_screen() {
        let point = AlacPoint::new(Line(0), Column(0));
        let mode = TermMode::ALT_SCREEN;

        let bytes = scroll_report(3, point, 0, mode);
        assert!(bytes.is_some());
        let sequence = bytes.unwrap();

        assert_eq!(sequence, b"\x1b[A\x1b[A\x1b[A");
    }

    #[test]
    fn test_scroll_report_app_cursor_conpty_tui() {
        let point = AlacPoint::new(Line(0), Column(0));

        let mode = TermMode::APP_CURSOR;

        let bytes = scroll_report(-2, point, 0, mode);
        assert!(bytes.is_some());
        let sequence = bytes.unwrap();
        assert_eq!(sequence, b"\x1bOB\x1bOB");

        let bytes = scroll_report(2, point, 0, mode);
        assert!(bytes.is_some());
        let sequence = bytes.unwrap();
        assert_eq!(sequence, b"\x1bOA\x1bOA");
    }

    #[test]
    fn test_scroll_report_normal_screen() {
        let point = AlacPoint::new(Line(0), Column(0));
        let mode = TermMode::empty();

        let bytes = scroll_report(3, point, 0, mode);
        assert!(bytes.is_none());
    }

    #[test]
    fn test_encode_modifiers() {
        assert_eq!(encode_modifiers(false, false, false), 0);
        assert_eq!(encode_modifiers(true, false, false), 4);
        assert_eq!(encode_modifiers(false, true, false), 8);
        assert_eq!(encode_modifiers(false, false, true), 16);
        assert_eq!(encode_modifiers(true, true, false), 12);
        assert_eq!(encode_modifiers(true, false, true), 20);
        assert_eq!(encode_modifiers(false, true, true), 24);
        assert_eq!(encode_modifiers(true, true, true), 28);
    }

    #[test]
    fn test_pixels_to_scroll_lines() {
        let cell_height = px(20.0);

        assert_eq!(pixels_to_scroll_lines(px(60.0), cell_height), 3);
        assert_eq!(pixels_to_scroll_lines(px(-40.0), cell_height), -2);
        assert_eq!(pixels_to_scroll_lines(px(10.0), cell_height), 1);
        assert_eq!(pixels_to_scroll_lines(px(-10.0), cell_height), -1);

        assert_eq!(pixels_to_scroll_lines(px(300.0), cell_height), 10);
        assert_eq!(pixels_to_scroll_lines(px(-300.0), cell_height), -10);
    }

    #[test]
    fn test_scroll_to_arrow_keys_limit() {
        let mode = TermMode::empty();

        let bytes = scroll_to_arrow_keys(100, mode);
        let expected = b"\x1b[A\x1b[A\x1b[A\x1b[A\x1b[A\x1b[A\x1b[A\x1b[A\x1b[A\x1b[A";
        assert_eq!(bytes, expected);

        let bytes = scroll_to_arrow_keys(-100, mode);
        let expected = b"\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B";
        assert_eq!(bytes, expected);
    }

    #[test]
    fn test_mouse_motion_report() {
        let point = AlacPoint::new(Line(3), Column(7));

        let mode_drag = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        let report = mouse_motion_report(point, Some(MouseButton::Left), 0, mode_drag);
        assert!(report.is_some());
        assert_eq!(String::from_utf8(report.unwrap()).unwrap(), "\x1b[<32;8;4M");

        let report = mouse_motion_report(point, None, 0, mode_drag);
        assert!(report.is_none());

        let mode_motion = TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE;
        let report = mouse_motion_report(point, None, 0, mode_motion);
        assert!(report.is_some());
        assert_eq!(String::from_utf8(report.unwrap()).unwrap(), "\x1b[<35;8;4M");

        let mode_click = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let report = mouse_motion_report(point, Some(MouseButton::Left), 0, mode_click);
        assert!(report.is_none());
    }
}
