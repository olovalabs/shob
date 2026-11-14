//! Responsive, cursor-reactive gradient ASCII welcome background.
use gpui::{
    App, Bounds, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    StatefulInteractiveElement, Styled, Window, canvas, div, px, rgba,
};
use std::cell::Cell;
use std::rc::Rc;

const CELL_WIDTH: f32 = 7.0;
const ROW_HEIGHT: f32 = 14.0;

fn line(row: usize, rows: usize, columns: usize, pointer: Option<(f32, f32)>) -> String {
    let y = row as f32 / rows.max(1) as f32;
    let glyphs: &[u8] = if y < 0.17 {
        b"8888SSS####@@"
    } else if y < 0.4 {
        b"SSSSS0000888"
    } else {
        b"xxXX000+++.."
    };
    let mut result = String::with_capacity(columns);
    for col in 0..columns {
        let mut seed =
            (col as u32).wrapping_mul(0x9e3779b9) ^ (row as u32).wrapping_mul(0x85ebca6b);
        if let Some((mx, my)) = pointer {
            let dx = col as f32 * CELL_WIDTH - mx;
            let dy = row as f32 * ROW_HEIGHT - my;
            if dx * dx + dy * dy < 80.0 * 80.0 {
                seed ^= ((mx / 4.0) as i32 as u32).wrapping_mul(0xc2b2ae35)
                    ^ ((my / 4.0) as i32 as u32).wrapping_mul(0x27d4eb2d);
            }
        }
        seed ^= seed >> 16;
        seed = seed.wrapping_mul(0x7feb352d);
        seed ^= seed >> 15;
        let empty = (seed % 100) < (y * 88.0) as u32;
        result.push(if empty {
            ' '
        } else {
            glyphs[(seed as usize >> 8) % glyphs.len()] as char
        });
    }
    result
}

fn color(x: f32, y: f32) -> gpui::Rgba {
    // The reference has cyan upper edges, a white central glow, royal blue
    // through the middle, and a near-black purple lower edge.
    let stops = [
        (0.0, [25.0, 186.0, 232.0]),
        (0.27, [24.0, 110.0, 213.0]),
        (0.55, [6.0, 22.0, 76.0]),
        (0.78, [16.0, 14.0, 23.0]),
        (1.0, [16.0, 14.0, 23.0]),
    ];
    let index = stops
        .windows(2)
        .position(|pair| y <= pair[1].0)
        .unwrap_or(stops.len() - 2);
    let (a, ca) = stops[index];
    let (b, cb) = stops[index + 1];
    let t = ((y - a) / (b - a)).clamp(0.0, 1.0);
    let white = (-((x - 0.5) / 0.27).powi(2)).exp() * (1.0 - y / 0.27).clamp(0.0, 1.0) * 0.88;
    let center_shadow = (-((x - 0.5) / 0.28).powi(2)).exp()
        * ((y - 0.37) / 0.35).clamp(0.0, 1.0)
        * (1.0 - ((y - 0.7) / 0.3).clamp(0.0, 1.0));
    let mut rgb = [0u8; 3];
    for i in 0..3 {
        let base = (ca[i] * (1.0 - t) + cb[i] * t) * (1.0 - center_shadow * 0.6);
        rgb[i] = (base + (255.0 - base) * white).clamp(0.0, 255.0) as u8;
    }
    gpui::rgb((rgb[0] as u32) << 16 | (rgb[1] as u32) << 8 | rgb[2] as u32)
}

pub fn render() -> impl IntoElement {
    let bounds = Rc::new(Cell::new(Bounds::<Pixels>::default()));
    let layout_bounds = bounds.clone();
    let move_bounds = bounds.clone();
    div()
        .id("theme-ascii-glow")
        .debug_selector(|| "theme-ascii-glow".into())
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .overflow_hidden()
        .on_hover(move |_, window, _| window.refresh())
        .on_mouse_move(move |event, window, _| {
            if move_bounds.get().contains(&event.position) {
                window.refresh();
            }
        })
        .child(
            canvas(
                move |bounds, _, _| {
                    layout_bounds.set(bounds);
                },
                move |bounds, _, window: &mut Window, cx: &mut App| {
                    let width: f32 = bounds.size.width.into();
                    let height: f32 = bounds.size.height.into();
                    if width <= 0.0 || height <= 0.0 {
                        return;
                    }
                    // Narrow horizontal bands use native linear gradients, keeping
                    // the two-dimensional glow smooth without thousands of quads.
                    let bands = 96;
                    let band_h = height / bands as f32;
                    for band in 0..bands {
                        let y = (band as f32 + 0.5) / bands as f32;
                        for half in 0..2 {
                            let x = half as f32 * 0.5;
                            let gradient = gpui::linear_gradient(
                                90.0,
                                gpui::linear_color_stop(color(x, y), 0.0),
                                gpui::linear_color_stop(color(x + 0.5, y), 1.0),
                            );
                            let rect = Bounds::new(
                                Point {
                                    x: bounds.origin.x + px(width * x),
                                    y: bounds.origin.y + px(band as f32 * band_h),
                                },
                                gpui::size(px(width * 0.5 + 1.0), px(band_h + 1.0)),
                            );
                            window.paint_quad(gpui::fill(rect, gradient));
                        }
                    }
                    let columns = ((width / CELL_WIDTH).ceil() as usize).min(500);
                    let rows = ((height * 0.78 / ROW_HEIGHT).ceil() as usize).clamp(1, 64);
                    let mouse = window.mouse_position();
                    let pointer = bounds.contains(&mouse).then(|| {
                        (
                            f32::from(mouse.x - bounds.origin.x),
                            f32::from(mouse.y - bounds.origin.y),
                        )
                    });
                    for row in 0..rows {
                        let text = line(row, rows, columns, pointer);
                        let y = row as f32 / rows as f32;
                        let fade = (1.0 - y).powf(1.35);
                        let mut runs = Vec::new();
                        let chunks = 12;
                        for chunk in 0..chunks {
                            let start = columns * chunk / chunks;
                            let end = columns * (chunk + 1) / chunks;
                            if end == start {
                                continue;
                            }
                            let x = (start + end) as f32 / (2.0 * columns.max(1) as f32);
                            let central = (-((x - 0.5) / 0.3).powi(2)).exp();
                            let mut ink: gpui::Hsla = rgba(0xc8f3ffff).into();
                            ink.a = (0.38 + central * 0.42) * fade;
                            runs.push(gpui::TextRun {
                                len: end - start,
                                font: gpui::font(crate::assets::MONO_FONT),
                                color: ink,
                                ..Default::default()
                            });
                        }
                        let shaped = window.text_system().shape_line(
                            text.into(),
                            px(10.0),
                            &runs,
                            Some(px(CELL_WIDTH)),
                        );
                        let _ = shaped.paint(
                            Point {
                                x: bounds.origin.x,
                                y: bounds.origin.y + px(row as f32 * ROW_HEIGHT),
                            },
                            px(ROW_HEIGHT),
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                },
            )
            .absolute()
            .size_full(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursor_scrambles_only_nearby_characters() {
        let original = line(2, 40, 120, None);
        let changed = line(2, 40, 120, Some((140.0, 28.0)));
        assert_ne!(&original[10..30], &changed[10..30]);
        assert_eq!(&original[60..], &changed[60..]);
    }
}
