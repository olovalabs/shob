use crate::colors::ColorPalette;
use crate::event::GpuiEventProxy;
use crate::shell_integration::{BlockLine, BlockRun};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column as GridColumn, Line as GridLine, Point as GridPoint};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::search::Match;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use gpui::{
    App, Bounds, Edges, Font, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, Point,
    SharedString, Size, StrikethroughStyle, TextRun, UnderlineStyle, Window, px, quad,
    transparent_black,
};
use parking_lot::Mutex;
use std::ops::RangeInclusive;
use std::sync::Arc;

/// Everything the renderer needs to know that does not live on the `Term`
/// itself: selection, search highlights, the hovered hyperlink, focus and
/// cursor-blink state.
pub struct PaintContext<'a> {
    /// Active selection, already resolved to grid coordinates (block aware).
    pub selection: Option<SelectionRange>,
    /// All search matches to highlight.
    pub search_matches: Option<&'a Arc<[Match]>>,
    /// A revision supplied by the grid owner; omit it when painting an untracked grid.
    pub screen_revision: Option<u64>,
    /// The currently focused search match, highlighted more strongly.
    pub active_match: Option<&'a Match>,
    /// Range of the hyperlink under the mouse, underlined while hovered.
    pub hovered_link: Option<&'a RangeInclusive<GridPoint>>,
    pub is_focused: bool,
    /// Current phase of the cursor blink animation.
    pub cursor_visible: bool,
    pub show_scrollbar: bool,
    /// The scrollbar is hovered or being dragged.
    pub scrollbar_active: bool,
    pub selection_color: Hsla,
    pub match_color: Hsla,
    pub active_match_color: Hsla,
}

impl Default for PaintContext<'_> {
    fn default() -> Self {
        Self {
            selection: None,
            search_matches: None,
            screen_revision: None,
            active_match: None,
            hovered_link: None,
            is_focused: true,
            cursor_visible: true,
            show_scrollbar: true,
            scrollbar_active: false,
            selection_color: gpui::hsla(215.0 / 360.0, 0.85, 0.45, 0.35),
            match_color: gpui::hsla(45.0 / 360.0, 0.9, 0.5, 0.30),
            active_match_color: gpui::hsla(25.0 / 360.0, 0.95, 0.55, 0.55),
        }
    }
}

/// Track a contiguous horizontal run of "flagged" cells (selected, matched,
/// hovered) so a whole run becomes one quad. `flush` is called with the
/// half-open `[start, end)` column range whenever a run ends.
fn accumulate_run<F: FnMut(usize, usize)>(
    run: &mut Option<(usize, usize)>,
    col: usize,
    active: bool,
    mut flush: F,
) {
    match run.take() {
        Some((start, end)) if active && end == col => *run = Some((start, col + 1)),
        Some((start, end)) => {
            flush(start, end);
            if active {
                *run = Some((col, col + 1));
            }
        }
        None => {
            if active {
                *run = Some((col, col + 1));
            }
        }
    }
}

/// RegexIter emits non-overlapping matches in grid order. Restrict a row's
/// highlight walk without scanning the entire scrollback match list per row.
fn row_match_range(matches: &[Match], line: i32) -> std::ops::Range<usize> {
    let start = matches.partition_point(|m| m.end().line.0 < line);
    let end = start + matches[start..].partition_point(|m| m.start().line.0 <= line);
    start..end
}

#[derive(Debug, Clone)]
pub struct BatchedTextRun {
    pub text: String,

    /// Number of `char`s in `text`, tracked while batching so the paint loop
    /// does not recount it (an O(n) scan) for every run of every frame.
    pub char_count: usize,

    pub cell_count: usize,

    pub start_col: usize,

    pub row: usize,

    pub fg_color: Hsla,

    pub bg_color: Hsla,

    pub bold: bool,

    pub italic: bool,

    pub underline: bool,

    pub strikethrough: bool,

    /// Draw the underline as a curl (`CSI 4:3 m` / undercurl).
    pub wavy_underline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackgroundRect {
    pub start_col: usize,

    pub end_col: usize,

    pub row: usize,

    pub end_row: usize,

    pub color: Hsla,
}

impl BackgroundRect {
    pub fn new(start_col: usize, end_col: usize, row: usize, color: Hsla) -> Self {
        Self {
            start_col,
            end_col,
            row,
            end_row: row + 1,
            color,
        }
    }

    pub fn can_merge_with(&self, other: &Self) -> bool {
        self.row == other.row
            && self.end_row == other.end_row
            && self.color == other.color
            && self.end_col == other.start_col
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockElementRect {
    pub bounds: Bounds<Pixels>,
    pub color: Hsla,
}

pub fn is_box_or_block(ch: char) -> bool {
    matches!(ch as u32, 0x2500..=0x257F | 0x2580..=0x259F)
}

pub fn is_decorative_symbol(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2500..=0x257F
        | 0x2580..=0x259F
        | 0x25A0..=0x25FF
        | 0x1FB00..=0x1FB3B
        | 0xE0B0..=0xE0D7
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rasterize_box_or_block(
    ch: char,
    col: usize,
    row: usize,
    color: Hsla,
    origin: Point<Pixels>,
    cell_width: Pixels,
    cell_height: Pixels,
    out: &mut Vec<BlockElementRect>,
) -> bool {
    macro_rules! emit {
        ($($rect:expr),+ $(,)?) => {{
            out.extend([$($rect),+]);
            true
        }};
    }
    let cx = origin.x + cell_width * (col as f32);
    let cy = origin.y + cell_height * (row as f32);
    let w: f32 = cell_width.into();
    let h: f32 = cell_height.into();

    let rect = |x_rel: f32, y_rel: f32, w_rel: f32, h_rel: f32| BlockElementRect {
        bounds: Bounds {
            origin: Point {
                x: cx + px(x_rel),
                y: cy + px(y_rel),
            },
            size: Size {
                width: px(w_rel),
                height: px(h_rel),
            },
        },
        color,
    };

    let code = ch as u32;

    match code {
        0x2580 => return emit![rect(0.0, 0.0, w, h * 0.5)],
        0x2581 => return emit![rect(0.0, h * 0.875, w, h * 0.125)],
        0x2582 => return emit![rect(0.0, h * 0.75, w, h * 0.25)],
        0x2583 => return emit![rect(0.0, h * 0.625, w, h * 0.375)],
        0x2584 => return emit![rect(0.0, h * 0.5, w, h * 0.5)],
        0x2585 => return emit![rect(0.0, h * 0.375, w, h * 0.625)],
        0x2586 => return emit![rect(0.0, h * 0.25, w, h * 0.75)],
        0x2587 => return emit![rect(0.0, h * 0.125, w, h * 0.875)],
        0x2588 => return emit![rect(0.0, 0.0, w, h)],
        0x2589 => return emit![rect(0.0, 0.0, w * 0.875, h)],
        0x258A => return emit![rect(0.0, 0.0, w * 0.75, h)],
        0x258B => return emit![rect(0.0, 0.0, w * 0.625, h)],
        0x258C => return emit![rect(0.0, 0.0, w * 0.5, h)],
        0x258D => return emit![rect(0.0, 0.0, w * 0.375, h)],
        0x258E => return emit![rect(0.0, 0.0, w * 0.25, h)],
        0x258F => return emit![rect(0.0, 0.0, w * 0.125, h)],
        0x2590 => return emit![rect(w * 0.5, 0.0, w * 0.5, h)],
        0x2591 => {
            return emit![BlockElementRect {
                bounds: Bounds {
                    origin: Point { x: cx, y: cy },
                    size: Size {
                        width: px(w),
                        height: px(h),
                    },
                },
                color: Hsla {
                    a: color.a * 0.25,
                    ..color
                },
            }];
        }
        0x2592 => {
            return emit![BlockElementRect {
                bounds: Bounds {
                    origin: Point { x: cx, y: cy },
                    size: Size {
                        width: px(w),
                        height: px(h),
                    },
                },
                color: Hsla {
                    a: color.a * 0.50,
                    ..color
                },
            }];
        }
        0x2593 => {
            return emit![BlockElementRect {
                bounds: Bounds {
                    origin: Point { x: cx, y: cy },
                    size: Size {
                        width: px(w),
                        height: px(h),
                    },
                },
                color: Hsla {
                    a: color.a * 0.75,
                    ..color
                },
            }];
        }
        0x2594 => return emit![rect(0.0, 0.0, w, h * 0.125)],
        0x2595 => return emit![rect(w * 0.875, 0.0, w * 0.125, h)],
        0x2596 => return emit![rect(0.0, h * 0.5, w * 0.5, h * 0.5)],
        0x2597 => return emit![rect(w * 0.5, h * 0.5, w * 0.5, h * 0.5)],
        0x2598 => return emit![rect(0.0, 0.0, w * 0.5, h * 0.5)],
        0x2599 => {
            return emit![
                rect(0.0, 0.0, w * 0.5, h),
                rect(w * 0.5, h * 0.5, w * 0.5, h * 0.5),
            ];
        }
        0x259A => {
            return emit![
                rect(0.0, 0.0, w * 0.5, h * 0.5),
                rect(w * 0.5, h * 0.5, w * 0.5, h * 0.5),
            ];
        }
        0x259B => {
            return emit![
                rect(0.0, 0.0, w, h * 0.5),
                rect(0.0, h * 0.5, w * 0.5, h * 0.5),
            ];
        }
        0x259C => {
            return emit![
                rect(0.0, 0.0, w, h * 0.5),
                rect(w * 0.5, h * 0.5, w * 0.5, h * 0.5),
            ];
        }
        0x259D => return emit![rect(w * 0.5, 0.0, w * 0.5, h * 0.5)],
        0x259E => {
            return emit![
                rect(w * 0.5, 0.0, w * 0.5, h * 0.5),
                rect(0.0, h * 0.5, w * 0.5, h * 0.5),
            ];
        }
        0x259F => {
            return emit![
                rect(0.0, h * 0.5, w, h * 0.5),
                rect(w * 0.5, 0.0, w * 0.5, h * 0.5),
            ];
        }
        _ => {}
    }

    let t = (h * 0.08).max(1.0).round();
    let ht = (h * 0.16).max(2.0).round();
    let mx = (w - t) * 0.5;
    let my = (h - t) * 0.5;
    let hmx = (w - ht) * 0.5;
    let hmy = (h - ht) * 0.5;

    match code {
        0x2500 | 0x2504 | 0x2508 => emit![rect(0.0, my, w, t)],
        0x2502 | 0x2506 | 0x250A => emit![rect(mx, 0.0, t, h)],
        0x250C | 0x256D => emit![rect(mx, my, w - mx, t), rect(mx, my, t, h - my)],
        0x2510 | 0x256E => emit![rect(0.0, my, mx + t, t), rect(mx, my, t, h - my)],
        0x2514 | 0x2570 => emit![rect(mx, my, w - mx, t), rect(mx, 0.0, t, my + t)],
        0x2518 | 0x256F => emit![rect(0.0, my, mx + t, t), rect(mx, 0.0, t, my + t)],
        0x251C => emit![rect(mx, 0.0, t, h), rect(mx, my, w - mx, t)],
        0x2524 => emit![rect(mx, 0.0, t, h), rect(0.0, my, mx + t, t)],
        0x252C => emit![rect(0.0, my, w, t), rect(mx, my, t, h - my)],
        0x2534 => emit![rect(0.0, my, w, t), rect(mx, 0.0, t, my + t)],
        0x253C => emit![rect(0.0, my, w, t), rect(mx, 0.0, t, h)],

        0x2501 | 0x2505 | 0x2509 => emit![rect(0.0, hmy, w, ht)],
        0x2503 | 0x2507 | 0x250B => emit![rect(hmx, 0.0, ht, h)],
        0x250F => emit![
            rect(hmx, hmy, w - hmx, ht),
            rect(hmx, hmy, ht, h - hmy),
        ],
        0x2513 => emit![
            rect(0.0, hmy, hmx + ht, ht),
            rect(hmx, hmy, ht, h - hmy),
        ],
        0x2517 => emit![
            rect(hmx, hmy, w - hmx, ht),
            rect(hmx, 0.0, ht, hmy + ht),
        ],
        0x251B => emit![
            rect(0.0, hmy, hmx + ht, ht),
            rect(hmx, 0.0, ht, hmy + ht),
        ],
        0x2523 => emit![rect(hmx, 0.0, ht, h), rect(hmx, hmy, w - hmx, ht)],
        0x252B => emit![rect(hmx, 0.0, ht, h), rect(0.0, hmy, hmx + ht, ht)],
        0x2533 => emit![rect(0.0, hmy, w, ht), rect(hmx, hmy, ht, h - hmy)],
        0x253B => emit![rect(0.0, hmy, w, ht), rect(hmx, 0.0, ht, hmy + ht)],
        0x254B => emit![rect(0.0, hmy, w, ht), rect(hmx, 0.0, ht, h)],

        0x2574 => emit![rect(0.0, my, mx + t, t)],
        0x2575 => emit![rect(mx, 0.0, t, my + t)],
        0x2576 => emit![rect(mx, my, w - mx, t)],
        0x2577 => emit![rect(mx, my, t, h - my)],
        0x2578 => emit![rect(0.0, hmy, hmx + ht, ht)],
        0x2579 => emit![rect(hmx, 0.0, ht, hmy + ht)],
        0x257A => emit![rect(hmx, hmy, w - hmx, ht)],
        0x257B => emit![rect(hmx, hmy, ht, h - hmy)],

        0x2550 => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(0.0, my - offset, w, t),
                rect(0.0, my + offset, w, t),
            ]
        }
        0x2551 => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(mx - offset, 0.0, t, h),
                rect(mx + offset, 0.0, t, h),
            ]
        }
        0x2554 => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(mx - offset, my - offset, w - (mx - offset), t),
                rect(mx + offset, my + offset, w - (mx + offset), t),
                rect(mx - offset, my - offset, t, h - (my - offset)),
                rect(mx + offset, my + offset, t, h - (my + offset)),
            ]
        }
        0x2557 => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(0.0, my - offset, mx + offset + t, t),
                rect(0.0, my + offset, mx - offset + t, t),
                rect(mx + offset, my - offset, t, h - (my - offset)),
                rect(mx - offset, my + offset, t, h - (my + offset)),
            ]
        }
        0x255A => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(mx - offset, my + offset, w - (mx - offset), t),
                rect(mx + offset, my - offset, w - (mx + offset), t),
                rect(mx - offset, 0.0, t, my + offset + t),
                rect(mx + offset, 0.0, t, my - offset + t),
            ]
        }
        0x255D => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(0.0, my + offset, mx + offset + t, t),
                rect(0.0, my - offset, mx - offset + t, t),
                rect(mx + offset, 0.0, t, my + offset + t),
                rect(mx - offset, 0.0, t, my - offset + t),
            ]
        }
        0x2560 => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(mx - offset, 0.0, t, h),
                rect(mx + offset, 0.0, t, h),
                rect(mx + offset, my - offset, w - (mx + offset), t),
                rect(mx + offset, my + offset, w - (mx + offset), t),
            ]
        }
        0x2563 => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(mx - offset, 0.0, t, h),
                rect(mx + offset, 0.0, t, h),
                rect(0.0, my - offset, mx - offset + t, t),
                rect(0.0, my + offset, mx - offset + t, t),
            ]
        }
        0x2566 => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(0.0, my - offset, w, t),
                rect(0.0, my + offset, w, t),
                rect(mx - offset, my + offset, t, h - (my + offset)),
                rect(mx + offset, my + offset, t, h - (my + offset)),
            ]
        }
        0x2569 => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(0.0, my - offset, w, t),
                rect(0.0, my + offset, w, t),
                rect(mx - offset, 0.0, t, my - offset + t),
                rect(mx + offset, 0.0, t, my - offset + t),
            ]
        }
        0x256C => {
            let offset = (t * 1.3).max(2.0);
            emit![
                rect(0.0, my - offset, w, t),
                rect(0.0, my + offset, w, t),
                rect(mx - offset, 0.0, t, h),
                rect(mx + offset, 0.0, t, h),
            ]
        }

        _ => false,
    }
}

fn clear_for_reuse<T>(buffer: &mut Vec<T>, expected_capacity: usize) {
    buffer.clear();
    let target = expected_capacity.max(64);
    if buffer.capacity() > target.saturating_mul(4) {
        buffer.shrink_to(target);
    }
}

struct PaintStamp {
    revision: u64,
    origin: Point<Pixels>,
    display_offset: usize,
    selection: Option<SelectionRange>,
    matches: Option<Arc<[Match]>>,
    active_match: Option<Match>,
    link: Option<RangeInclusive<GridPoint>>,
    highlight_colors: [Hsla; 3],
}

struct PreparedTextRun {
    text: SharedString,
    style: TextRun,
    origin: Point<Pixels>,
    force_width: Option<Pixels>,
}

#[derive(Default)]
struct PaintScratch {
    stamp: Option<PaintStamp>,
    row_backgrounds: Vec<BackgroundRect>,
    text_runs: Vec<BatchedTextRun>,
    prepared_text: Vec<PreparedTextRun>,
    text_bytes: usize,
    block_rects: Vec<BlockElementRect>,
    selection_rects: Vec<BlockElementRect>,
    match_rects: Vec<BlockElementRect>,
    decoration_rects: Vec<BlockElementRect>,
}

#[derive(Clone)]
pub struct TerminalRenderer {
    pub font_family: SharedString,

    pub font_size: Pixels,

    pub cell_width: Pixels,

    pub cell_height: Pixels,

    pub line_height_multiplier: f32,

    pub palette: ColorPalette,

    /// Reuses per-frame vectors across paints instead of allocating a fresh set
    /// on every terminal redraw. `TerminalRenderer` is cloned into GPUI's paint
    /// closure, so the scratch is shared by those short-lived clones.
    paint_scratch: Arc<Mutex<PaintScratch>>,
}

impl TerminalRenderer {
    pub fn new(
        font_family: String,
        font_size: Pixels,
        line_height_multiplier: f32,
        palette: ColorPalette,
    ) -> Self {
        let cell_width = font_size * 0.6;
        let cell_height = font_size * 1.4;

        Self {
            font_family: font_family.into(),
            font_size,
            cell_width,
            cell_height,
            line_height_multiplier,
            palette,
            paint_scratch: Arc::new(Mutex::new(PaintScratch::default())),
        }
    }

    pub(crate) fn release_idle_resources(&self) {
        *self.paint_scratch.lock() = PaintScratch::default();
    }

    /// Bytes held by the reusable paint scratch. This is the render cache a
    /// hibernated pane releases, and it is reported so the diagnostics line
    /// does not have to guess.
    pub fn memory_usage(&self) -> usize {
        let scratch = self.paint_scratch.lock();
        scratch.row_backgrounds.capacity() * std::mem::size_of::<BackgroundRect>()
            + scratch.text_runs.capacity() * std::mem::size_of::<BatchedTextRun>()
            + scratch.prepared_text.capacity() * std::mem::size_of::<PreparedTextRun>()
            + scratch.text_bytes
            + (scratch.block_rects.capacity()
                + scratch.selection_rects.capacity()
                + scratch.match_rects.capacity()
                + scratch.decoration_rects.capacity())
                * std::mem::size_of::<BlockElementRect>()
    }

    pub fn measure_cell(&mut self, window: &mut Window) {
        let font = Font {
            family: self.font_family.clone(),
            features: FontFeatures::default(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };

        let text_run = TextRun {
            len: 1,
            font,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        let shaped = window
            .text_system()
            .shape_line("M".into(), self.font_size, &[text_run], None);

        if shaped.width > px(0.0) {
            self.cell_width = shaped.width;
        }

        let line_height = shaped.ascent + shaped.descent;
        if line_height > px(0.0) {
            self.cell_height = line_height * self.line_height_multiplier;
        }
    }

    pub fn layout_row(
        &self,
        row: usize,
        cells: impl Iterator<Item = (usize, Cell)>,
        colors: &Colors,
    ) -> (Vec<BackgroundRect>, Vec<BatchedTextRun>) {
        let mut backgrounds = Vec::new();
        let mut text_runs = Vec::new();

        let mut current_run: Option<BatchedTextRun> = None;
        let mut current_bg: Option<BackgroundRect> = None;

        for (col, cell) in cells {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }

            let mut fg_color = self.palette.resolve(cell.fg, colors);
            let mut bg_color = self.palette.resolve(cell.bg, colors);

            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg_color, &mut bg_color);
            }
            bg_color = self.palette.map_background(bg_color);

            if cell.flags.contains(Flags::DIM) {
                fg_color.a *= 0.65;
            }

            if cell.flags.contains(Flags::HIDDEN) {
                fg_color = gpui::transparent_black();
            }

            let bold = cell.flags.contains(Flags::BOLD);
            let italic = cell.flags.contains(Flags::ITALIC);
            let underline = cell.flags.contains(Flags::UNDERLINE);
            let strikethrough = cell.flags.contains(Flags::STRIKEOUT);

            let ch = if cell.c == ' ' || cell.c == '\0' {
                ' '
            } else {
                cell.c
            };

            let cell_width_units = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };

            if let Some(ref mut bg_rect) = current_bg {
                if bg_rect.color == bg_color && bg_rect.end_col == col {
                    bg_rect.end_col = col + 1;
                } else {
                    let finished = std::mem::replace(
                        bg_rect,
                        BackgroundRect::new(col, col + 1, row, bg_color),
                    );
                    backgrounds.push(finished);
                }
            } else {
                current_bg = Some(BackgroundRect::new(col, col + 1, row, bg_color));
            }

            let zerowidth = cell.zerowidth().unwrap_or_default();
            let cell_char_count = 1 + zerowidth.len();
            let mergeable = current_run.as_ref().is_some_and(|run| {
                run.fg_color == fg_color
                    && run.bg_color == bg_color
                    && run.bold == bold
                    && run.italic == italic
                    && run.underline == underline
                    && run.strikethrough == strikethrough
                    && run.start_col + run.cell_count == col
            });
            if mergeable {
                let run = current_run.as_mut().unwrap();
                run.text.push(ch);
                run.text.extend(zerowidth.iter().copied());
                run.char_count += cell_char_count;
                run.cell_count += cell_width_units;
            } else {
                if let Some(run) = current_run.take() {
                    text_runs.push(run);
                }
                // Allocate once per styled run, not once per cell. Snapshotting
                // a plain 120-column line previously allocated 120 tiny Strings.
                let mut text = String::with_capacity(16);
                text.push(ch);
                text.extend(zerowidth.iter().copied());
                current_run = Some(BatchedTextRun {
                    text,
                    char_count: cell_char_count,
                    cell_count: cell_width_units,
                    start_col: col,
                    row,
                    fg_color,
                    bg_color,
                    bold,
                    italic,
                    underline,
                    strikethrough,
                    wavy_underline: false,
                });
            }
        }

        if let Some(run) = current_run {
            text_runs.push(run);
        }
        if let Some(bg) = current_bg {
            backgrounds.push(bg);
        }

        let merged_backgrounds = self.merge_backgrounds(backgrounds);

        (merged_backgrounds, text_runs)
    }

    /// Snapshot one terminal row as immutable block runs.
    ///
    /// Reuses the exact cell styling and run-batching used by the live painter,
    /// so captured output keeps ANSI colours, bold/italic, underlines and wide
    /// characters. Trailing spaces are trimmed because a block lays its own
    /// background and width.
    pub fn snapshot_line(
        &self,
        row: usize,
        cells: impl Iterator<Item = (usize, Cell)>,
        colors: &Colors,
    ) -> BlockLine {
        let (_, runs) = self.layout_row(row, cells, colors);
        let default_bg = self
            .palette
            .resolve(Color::Named(NamedColor::Background), colors);

        let mut out = Vec::with_capacity(runs.len());
        let mut total_cells = 0usize;
        for run in runs {
            total_cells += run.cell_count;
            out.push(BlockRun {
                text: run.text,
                fg: run.fg_color,
                bg: if run.bg_color == default_bg {
                    None
                } else {
                    Some(run.bg_color)
                },
                bold: run.bold,
                italic: run.italic,
                underline: run.underline,
                strikethrough: run.strikethrough,
                wavy: run.wavy_underline,
            });
        }

        while let Some(last) = out.last_mut() {
            let trimmed = last.text.trim_end_matches(' ');
            let removed = last.text.chars().count() - trimmed.chars().count();
            if removed > 0 {
                last.text.truncate(trimmed.len());
                total_cells = total_cells.saturating_sub(removed);
            }
            if last.text.is_empty() {
                out.pop();
            } else {
                break;
            }
        }

        BlockLine {
            runs: out,
            cells: total_cells,
        }
    }

    pub fn merge_backgrounds(&self, mut rects: Vec<BackgroundRect>) -> Vec<BackgroundRect> {
        if rects.is_empty() {
            return rects;
        }

        let mut merged = Vec::new();
        let mut current = rects.remove(0);

        for rect in rects {
            if current.can_merge_with(&rect) {
                current.end_col = rect.end_col;
            } else {
                merged.push(current);
                current = rect;
            }
        }

        merged.push(current);
        merged
    }

    pub fn merge_backgrounds_2d(mut rects: Vec<BackgroundRect>) -> Vec<BackgroundRect> {
        Self::merge_backgrounds_2d_in_place(&mut rects);
        rects
    }

    /// Merge vertically adjacent background runs in place. The old scan looked
    /// through every previously emitted rectangle for every new run (quadratic
    /// for highly-coloured output); sorting groups compatible runs so the merge
    /// itself is linear and reuses the caller's allocation.
    fn merge_backgrounds_2d_in_place(rects: &mut Vec<BackgroundRect>) {
        if rects.len() < 2 {
            return;
        }

        rects.sort_unstable_by(|a, b| {
            a.start_col
                .cmp(&b.start_col)
                .then_with(|| a.end_col.cmp(&b.end_col))
                .then_with(|| a.color.cmp(&b.color))
                .then_with(|| a.row.cmp(&b.row))
        });

        let mut write = 0;
        for read in 1..rects.len() {
            let rect = rects[read];
            let previous = rects[write];
            if previous.color == rect.color
                && previous.start_col == rect.start_col
                && previous.end_col == rect.end_col
                && previous.end_row == rect.row
            {
                rects[write].end_row = rect.end_row;
            } else {
                write += 1;
                rects[write] = rect;
            }
        }
        rects.truncate(write + 1);
    }

    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        padding: Edges<Pixels>,
        term: &Term<GpuiEventProxy>,
        ctx: &PaintContext<'_>,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let diag_paint = crate::diagnostics::paint_start();
        let grid = term.grid();
        let num_lines = grid.screen_lines();
        let num_cols = grid.columns();
        let colors = term.colors();
        let mode = *term.mode();

        let default_bg = self
            .palette
            .resolve(Color::Named(NamedColor::Background), colors);

        window.paint_quad(quad(
            bounds,
            px(0.0),
            default_bg,
            Edges::<Pixels>::default(),
            transparent_black(),
            Default::default(),
        ));

        let origin = Point {
            x: bounds.origin.x + padding.left,
            y: bounds.origin.y + padding.top,
        };

        // In the alternate screen there is no scrollback, so the viewport is
        // always pinned to the active grid.
        let display_offset = if mode.contains(TermMode::ALT_SCREEN) {
            0
        } else {
            grid.display_offset()
        };

        let cell_width = self.cell_width;
        let cell_height = self.cell_height;

        let cell_rect = move |col: usize, row: usize, span: usize, color: Hsla| BlockElementRect {
            bounds: Bounds {
                origin: Point {
                    x: origin.x + cell_width * (col as f32),
                    y: origin.y + cell_height * (row as f32),
                },
                size: Size {
                    width: cell_width * (span as f32),
                    height: cell_height,
                },
            },
            color,
        };

        let line_rect = move |col: usize,
                              row: usize,
                              span_cells: f32,
                              from_bottom: f32,
                              thickness: f32,
                              color: Hsla| BlockElementRect {
            bounds: Bounds {
                origin: Point {
                    x: origin.x + cell_width * (col as f32),
                    y: origin.y + cell_height * (row as f32) + cell_height - px(from_bottom),
                },
                size: Size {
                    width: cell_width * span_cells,
                    height: px(thickness),
                },
            },
            color,
        };

        // Reuse all frame-local vectors across paints. Capacity is retained for
        // stable terminal sizes, but exceptionally large windows/searches do not
        // pin that high-water allocation after the view shrinks.
        let cell_count = num_lines.saturating_mul(num_cols);
        let mut scratch = self.paint_scratch.lock();
        let highlight_colors = [ctx.selection_color, ctx.match_color, ctx.active_match_color];
        let reuse = scratch.stamp.as_ref().is_some_and(|stamp| {
            ctx.screen_revision == Some(stamp.revision)
                && stamp.origin == origin
                && stamp.display_offset == display_offset
                && stamp.selection == ctx.selection
                && match (stamp.matches.as_ref(), ctx.search_matches) {
                    (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
                    (None, None) => true,
                    _ => false,
                }
                && stamp.active_match.as_ref() == ctx.active_match
                && stamp.link.as_ref() == ctx.hovered_link
                && stamp.highlight_colors == highlight_colors
        });
        if !reuse {
            scratch.stamp = ctx.screen_revision.map(|revision| PaintStamp {
                revision,
                origin,
                display_offset,
                selection: ctx.selection,
                matches: ctx.search_matches.cloned(),
                active_match: ctx.active_match.cloned(),
                link: ctx.hovered_link.cloned(),
                highlight_colors,
            });
        }
        let PaintScratch {
            stamp: _,
            row_backgrounds,
            text_runs: batched_text_runs,
            prepared_text,
            text_bytes,
            block_rects,
            selection_rects,
            match_rects,
            decoration_rects,
        } = &mut *scratch;
        if !reuse {
            let search_matches: &[Match] = ctx.search_matches.map_or(&[], |matches| matches);
            clear_for_reuse(row_backgrounds, cell_count);
            clear_for_reuse(batched_text_runs, cell_count);
            clear_for_reuse(block_rects, cell_count);
            clear_for_reuse(selection_rects, cell_count);
            clear_for_reuse(match_rects, cell_count);
            clear_for_reuse(decoration_rects, cell_count);

            let underline_thickness = ((f32::from(self.font_size) * 0.07).round()).max(1.0);

            for line_idx in 0..num_lines {
                let buffer_line = (line_idx as i32) - (display_offset as i32);

                // A binary-search range preserves multiline matches crossing this
                // row, while ignoring thousands of offscreen scrollback matches.
                let row_matches = &search_matches[row_match_range(search_matches, buffer_line)];
                let mut match_cursor = 0;

                let mut current_bg: Option<BackgroundRect> = None;
                let mut current_run: Option<BatchedTextRun> = None;

                // Track contiguous runs of selected / matched / hovered cells so we
                // emit one quad per run instead of one per cell.
                let mut selection_run: Option<(usize, usize)> = None;
                let mut match_run: Option<(usize, usize, bool)> = None;
                let mut link_run: Option<(usize, usize)> = None;

                for col_idx in 0..num_cols {
                    let point = GridPoint::new(GridLine(buffer_line), GridColumn(col_idx));
                    let cell = &grid[point];
                    let ch = if cell.c == ' ' || cell.c == '\0' {
                        ' '
                    } else {
                        cell.c
                    };

                    let mut fg_color = self.palette.resolve(cell.fg, colors);
                    let mut bg_color = self.palette.resolve(cell.bg, colors);

                    if cell.flags.contains(Flags::INVERSE) {
                        std::mem::swap(&mut fg_color, &mut bg_color);
                    }
                    bg_color = self.palette.map_background(bg_color);

                    // `DIM_BOLD` means "dim applied to a bold cell": alacritty
                    // encodes it as DIM | BOLD, so checking DIM alone is correct.
                    if cell.flags.contains(Flags::DIM) {
                        fg_color.a *= 0.65;
                    }

                    if cell.flags.contains(Flags::HIDDEN) {
                        fg_color = gpui::transparent_black();
                    }

                    let bold = cell.flags.contains(Flags::BOLD);
                    let italic = cell.flags.contains(Flags::ITALIC);
                    let strikethrough = cell.flags.contains(Flags::STRIKEOUT);

                    // -- selection / search / link decorations -------------------
                    let is_selected = ctx
                        .selection
                        .as_ref()
                        .map(|range| range.contains(point))
                        .unwrap_or(false);
                    accumulate_run(&mut selection_run, col_idx, is_selected, |start, end| {
                        selection_rects.push(cell_rect(
                            start,
                            line_idx,
                            end - start,
                            ctx.selection_color,
                        ));
                    });

                    let matched = loop {
                        let Some(candidate) = row_matches.get(match_cursor) else {
                            break None;
                        };
                        if candidate.contains(&point) {
                            break Some(candidate);
                        }
                        // Matches are produced in grid order by RegexIter. Once a
                        // match starts before this cell and no longer contains it,
                        // it can never match a later column on this row.
                        if *candidate.start() > point {
                            break None;
                        }
                        match_cursor += 1;
                    };
                    let match_state =
                        matched.map(|matched| ctx.active_match.is_some_and(|active| active == matched));
                    match match_state {
                        Some(is_active) => {
                            let extend = match match_run {
                                Some((start, end, active)) if active == is_active && end == col_idx => {
                                    Some((start, col_idx + 1, active))
                                }
                                _ => None,
                            };
                            if let Some(next) = extend {
                                match_run = Some(next);
                            } else {
                                if let Some((start, end, active)) = match_run.take() {
                                    match_rects.push(cell_rect(
                                        start,
                                        line_idx,
                                        end - start,
                                        if active {
                                            ctx.active_match_color
                                        } else {
                                            ctx.match_color
                                        },
                                    ));
                                }
                                match_run = Some((col_idx, col_idx + 1, is_active));
                            }
                        }
                        None => {
                            if let Some((start, end, active)) = match_run.take() {
                                match_rects.push(cell_rect(
                                    start,
                                    line_idx,
                                    end - start,
                                    if active {
                                        ctx.active_match_color
                                    } else {
                                        ctx.match_color
                                    },
                                ));
                            }
                        }
                    }

                    let is_link = ctx
                        .hovered_link
                        .map(|range| range.contains(&point))
                        .unwrap_or(false);
                    accumulate_run(&mut link_run, col_idx, is_link, |start, end| {
                        decoration_rects.push(line_rect(
                            start,
                            line_idx,
                            (end - start) as f32,
                            underline_thickness + 1.0,
                            underline_thickness,
                            fg_color,
                        ));
                    });

                    // -- background ---------------------------------------------
                    if bg_color != default_bg {
                        if let Some(ref mut bg) = current_bg {
                            if bg.color == bg_color && bg.end_col == col_idx {
                                bg.end_col = col_idx + 1;
                            } else {
                                let finished = std::mem::replace(
                                    bg,
                                    BackgroundRect::new(col_idx, col_idx + 1, line_idx, bg_color),
                                );
                                row_backgrounds.push(finished);
                            }
                        } else {
                            current_bg = Some(BackgroundRect::new(
                                col_idx,
                                col_idx + 1,
                                line_idx,
                                bg_color,
                            ));
                        }
                    } else if let Some(bg) = current_bg.take() {
                        row_backgrounds.push(bg);
                    }

                    // The trailing half of a double-width glyph carries no content.
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                        || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }

                    // -- underline / strikethrough decorations -------------------
                    let underline_color = cell
                        .underline_color()
                        .map(|color| self.palette.resolve(color, colors));
                    let cell_span = if cell.flags.contains(Flags::WIDE_CHAR) {
                        2usize
                    } else {
                        1usize
                    };
                    let deco_color = underline_color.unwrap_or(fg_color);

                    // GPUI can draw a straight or a wavy underline as part of the
                    // text run; everything else (double / dotted / dashed, or a
                    // custom colour) is painted as quads.
                    let mut run_underline = false;
                    let mut run_wavy = false;

                    if cell.flags.contains(Flags::DOUBLE_UNDERLINE) {
                        decoration_rects.push(line_rect(
                            col_idx,
                            line_idx,
                            cell_span as f32,
                            underline_thickness * 3.0 + 1.0,
                            underline_thickness,
                            deco_color,
                        ));
                        decoration_rects.push(line_rect(
                            col_idx,
                            line_idx,
                            cell_span as f32,
                            underline_thickness + 1.0,
                            underline_thickness,
                            deco_color,
                        ));
                    } else if cell.flags.contains(Flags::UNDERCURL) {
                        if underline_color.is_some() {
                            decoration_rects.push(line_rect(
                                col_idx,
                                line_idx,
                                cell_span as f32,
                                underline_thickness + 1.0,
                                underline_thickness,
                                deco_color,
                            ));
                        } else {
                            run_underline = true;
                            run_wavy = true;
                        }
                    } else if cell.flags.contains(Flags::DOTTED_UNDERLINE) {
                        decoration_rects.push(line_rect(
                            col_idx,
                            line_idx,
                            cell_span as f32 * 0.25,
                            underline_thickness + 1.0,
                            underline_thickness,
                            deco_color,
                        ));
                        decoration_rects.push(line_rect(
                            col_idx,
                            line_idx,
                            cell_span as f32 * 0.25,
                            underline_thickness + 1.0,
                            underline_thickness,
                            deco_color,
                        ));
                    } else if cell.flags.contains(Flags::DASHED_UNDERLINE) {
                        decoration_rects.push(line_rect(
                            col_idx,
                            line_idx,
                            cell_span as f32 * 0.6,
                            underline_thickness + 1.0,
                            underline_thickness,
                            deco_color,
                        ));
                    } else if cell.flags.contains(Flags::UNDERLINE) {
                        if underline_color.is_some() {
                            decoration_rects.push(line_rect(
                                col_idx,
                                line_idx,
                                cell_span as f32,
                                underline_thickness + 1.0,
                                underline_thickness,
                                deco_color,
                            ));
                        } else {
                            run_underline = true;
                        }
                    }

                    // -- box drawing --------------------------------------------
                    // Most terminal cells are plain text; avoid the geometry
                    // rasterizer's range checks and coordinate math for them.
                    if is_box_or_block(ch)
                        && rasterize_box_or_block(
                            ch,
                            col_idx,
                            line_idx,
                            fg_color,
                            origin,
                            self.cell_width,
                            self.cell_height,
                            block_rects,
                        )
                    {
                        if let Some(run) = current_run.take() {
                            batched_text_runs.push(run);
                        }
                        continue;
                    }

                    let is_blank = ch == ' '
                        && bg_color == default_bg
                        && !run_underline
                        && !strikethrough
                        && !cell.flags.contains(Flags::INVERSE);

                    if is_blank {
                        if let Some(run) = current_run.take() {
                            batched_text_runs.push(run);
                        }
                        continue;
                    }

                    let is_wide = cell.flags.contains(Flags::WIDE_CHAR);
                    let has_zerowidth = cell.zerowidth().is_some();

                    if is_wide || has_zerowidth {
                        if let Some(run) = current_run.take() {
                            batched_text_runs.push(run);
                        }
                        // Wide and zero-width-joiner cells are never merged, so
                        // their one-off String is only built on this rare path —
                        // the common single-char path below pushes `ch` directly
                        // into the running buffer instead of allocating per cell.
                        let mut cell_text = String::with_capacity(ch.len_utf8() + 8);
                        cell_text.push(ch);
                        let mut char_count = 1;
                        if let Some(zerowidth) = cell.zerowidth() {
                            for &zc in zerowidth {
                                cell_text.push(zc);
                                char_count += 1;
                            }
                        }
                        batched_text_runs.push(BatchedTextRun {
                            text: cell_text,
                            char_count,
                            cell_count: cell_span,
                            start_col: col_idx,
                            row: line_idx,
                            fg_color,
                            bg_color,
                            bold,
                            italic,
                            underline: run_underline,
                            strikethrough,
                            wavy_underline: run_wavy,
                        });
                        continue;
                    }

                    let mergeable = current_run.as_ref().is_some_and(|run| {
                        run.fg_color == fg_color
                            && run.bold == bold
                            && run.italic == italic
                            && run.underline == run_underline
                            && run.wavy_underline == run_wavy
                            && run.strikethrough == strikethrough
                            && run.start_col + run.cell_count == col_idx
                    });

                    if mergeable {
                        if let Some(ref mut run) = current_run {
                            run.text.push(ch);
                            run.char_count += 1;
                            run.cell_count += cell_span;
                        }
                    } else {
                        if let Some(run) = current_run.take() {
                            batched_text_runs.push(run);
                        }
                        // Wide chars took the branch above, so this cell is exactly
                        // one char in one column.
                        let mut text = String::with_capacity(16);
                        text.push(ch);
                        current_run = Some(BatchedTextRun {
                            text,
                            char_count: 1,
                            cell_count: cell_span,
                            start_col: col_idx,
                            row: line_idx,
                            fg_color,
                            bg_color,
                            bold,
                            italic,
                            underline: run_underline,
                            strikethrough,
                            wavy_underline: run_wavy,
                        });
                    }
                }

                if let Some((start, end)) = selection_run.take() {
                    selection_rects.push(cell_rect(start, line_idx, end - start, ctx.selection_color));
                }
                if let Some((start, end, active)) = match_run.take() {
                    match_rects.push(cell_rect(
                        start,
                        line_idx,
                        end - start,
                        if active {
                            ctx.active_match_color
                        } else {
                            ctx.match_color
                        },
                    ));
                }
                if let Some((start, end)) = link_run.take() {
                    let color = self
                        .palette
                        .resolve(Color::Named(NamedColor::Foreground), colors);
                    decoration_rects.push(line_rect(
                        start,
                        line_idx,
                        (end - start) as f32,
                        underline_thickness + 1.0,
                        underline_thickness,
                        color,
                    ));
                }
                if let Some(bg) = current_bg {
                    row_backgrounds.push(bg);
                }
                if let Some(run) = current_run {
                    batched_text_runs.push(run);
                }
            }
            Self::merge_backgrounds_2d_in_place(row_backgrounds);
            clear_for_reuse(prepared_text, batched_text_runs.len());
            *text_bytes = 0;
            let font_family = &self.font_family;
            let font_features = FontFeatures::disable_ligatures();
            for run in batched_text_runs.drain(..) {
                let x = origin.x + self.cell_width * (run.start_col as f32);
                let y = origin.y + self.cell_height * (run.row as f32);

                let font = Font {
                    family: font_family.clone(),
                    features: font_features.clone(),
                    fallbacks: None,
                    weight: if run.bold {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    },
                    style: if run.italic {
                        FontStyle::Italic
                    } else {
                        FontStyle::Normal
                    },
                };

                let char_len = run.text.len();
                let text_run = TextRun {
                    len: char_len,
                    font,
                    color: run.fg_color,
                    background_color: None,
                    underline: if run.underline {
                        Some(UnderlineStyle {
                            thickness: px(underline_thickness),
                            color: Some(run.fg_color),
                            wavy: run.wavy_underline,
                        })
                    } else {
                        None
                    },
                    strikethrough: if run.strikethrough {
                        Some(StrikethroughStyle {
                            thickness: px(underline_thickness),
                            color: Some(run.fg_color),
                        })
                    } else {
                        None
                    },
                };

                let force_width = if run.cell_count > 1 || run.char_count != run.cell_count {
                    None
                } else {
                    Some(self.cell_width)
                };
                *text_bytes += char_len;
                prepared_text.push(PreparedTextRun {
                    text: run.text.into(),
                    style: text_run,
                    origin: Point { x, y },
                    force_width,
                });
            }
        }

        // -- paint order: cell backgrounds, blocks, search, selection, text --
        for bg in row_backgrounds.iter().copied() {
            let x = origin.x + self.cell_width * (bg.start_col as f32);
            let y = origin.y + self.cell_height * (bg.row as f32);
            let width = self.cell_width * ((bg.end_col - bg.start_col) as f32);
            let height = self.cell_height * ((bg.end_row - bg.row) as f32);

            window.paint_quad(quad(
                Bounds {
                    origin: Point { x, y },
                    size: Size { width, height },
                },
                px(0.0),
                bg.color,
                Edges::<Pixels>::default(),
                transparent_black(),
                Default::default(),
            ));
        }

        for block in block_rects.iter() {
            window.paint_quad(quad(
                block.bounds,
                px(0.0),
                block.color,
                Edges::<Pixels>::default(),
                transparent_black(),
                Default::default(),
            ));
        }

        for rect in match_rects.iter().chain(selection_rects.iter()) {
            window.paint_quad(quad(
                rect.bounds,
                px(0.0),
                rect.color,
                Edges::<Pixels>::default(),
                transparent_black(),
                Default::default(),
            ));
        }

        for run in prepared_text.iter() {
            let shaped_line = window.text_system().shape_line(
                run.text.clone(),
                self.font_size,
                std::slice::from_ref(&run.style),
                run.force_width,
            );

            let _ = shaped_line.paint(
                run.origin,
                self.cell_height,
                gpui::TextAlign::Left,
                None,
                window,
                _cx,
            );
        }

        for rect in decoration_rects.iter() {
            window.paint_quad(quad(
                rect.bounds,
                px(0.0),
                rect.color,
                Edges::<Pixels>::default(),
                transparent_black(),
                Default::default(),
            ));
        }

        // -- cursor ----------------------------------------------------------
        self.paint_cursor(term, ctx, origin, display_offset, num_lines, colors, window);

        // -- scrollbar --------------------------------------------------------
        let history_size = grid.history_size();
        if ctx.show_scrollbar && history_size > 0 {
            let total_lines = (history_size + num_lines) as f32;
            let available_h: f32 = bounds.size.height.into();
            let thumb_h = (available_h * (num_lines as f32 / total_lines)).max(24.0);
            let scroll_fraction = (history_size - display_offset) as f32 / (history_size as f32);
            let thumb_y = bounds.origin.y + px((available_h - thumb_h) * scroll_fraction);
            let thumb_w = if ctx.scrollbar_active {
                px(8.0)
            } else {
                px(4.0)
            };
            let thumb_x = bounds.origin.x + bounds.size.width - thumb_w - px(2.0);

            let thumb_color = if ctx.scrollbar_active {
                gpui::rgba(0xffffff99)
            } else if display_offset > 0 {
                gpui::rgba(0xffffff66)
            } else {
                gpui::rgba(0xffffff1a)
            };

            window.paint_quad(quad(
                Bounds {
                    origin: Point {
                        x: thumb_x,
                        y: thumb_y,
                    },
                    size: Size {
                        width: thumb_w,
                        height: px(thumb_h),
                    },
                },
                px(2.0),
                thumb_color,
                Edges::<Pixels>::default(),
                transparent_black(),
                Default::default(),
            ));
        }

        crate::diagnostics::paint_end(diag_paint);
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_cursor(
        &self,
        term: &Term<GpuiEventProxy>,
        ctx: &PaintContext<'_>,
        origin: Point<Pixels>,
        display_offset: usize,
        num_lines: usize,
        colors: &Colors,
        window: &mut Window,
    ) {
        let mode = *term.mode();
        let vi_mode = mode.contains(TermMode::VI);

        // In vi mode the vi cursor is what the user is driving; the real cursor
        // is drawn hollow underneath it (this is what alacritty and Zed do).
        let cursor_point = if vi_mode {
            term.vi_mode_cursor.point
        } else {
            term.grid().cursor.point
        };

        let cursor_screen_line = cursor_point.line.0 + display_offset as i32;
        if cursor_screen_line < 0 || cursor_screen_line as usize >= num_lines {
            return;
        }

        if !vi_mode && !mode.contains(TermMode::SHOW_CURSOR) {
            return;
        }

        let cursor_style = term.cursor_style();
        if !vi_mode && cursor_style.shape == CursorShape::Hidden {
            return;
        }

        // Suppress cursor when ctx.cursor_visible is false (read-only / rich input mode, or blink phase)
        if !vi_mode && !ctx.cursor_visible {
            return;
        }

        let cursor_color = self
            .palette
            .resolve(Color::Named(NamedColor::Cursor), colors);

        let cursor_x = origin.x + self.cell_width * (cursor_point.column.0 as f32);
        let cursor_y = origin.y + self.cell_height * (cursor_screen_line as f32);

        let hollow = !ctx.is_focused || cursor_style.shape == CursorShape::HollowBlock;
        let shape = if vi_mode {
            CursorShape::Block
        } else {
            cursor_style.shape
        };

        if hollow {
            window.paint_quad(quad(
                Bounds {
                    origin: Point {
                        x: cursor_x,
                        y: cursor_y,
                    },
                    size: Size {
                        width: self.cell_width,
                        height: self.cell_height,
                    },
                },
                px(0.0),
                transparent_black(),
                Edges::all(px(1.5)),
                cursor_color,
                Default::default(),
            ));
            return;
        }

        let cursor_bounds = match shape {
            CursorShape::Beam => Bounds {
                origin: Point {
                    x: cursor_x,
                    y: cursor_y,
                },
                size: Size {
                    width: px(2.0),
                    height: self.cell_height,
                },
            },
            CursorShape::Underline => Bounds {
                origin: Point {
                    x: cursor_x,
                    y: cursor_y + self.cell_height - px(2.0),
                },
                size: Size {
                    width: self.cell_width,
                    height: px(2.0),
                },
            },
            _ => Bounds {
                origin: Point {
                    x: cursor_x,
                    y: cursor_y,
                },
                size: Size {
                    width: self.cell_width,
                    height: self.cell_height,
                },
            },
        };

        window.paint_quad(quad(
            cursor_bounds,
            px(0.0),
            cursor_color,
            Edges::<Pixels>::default(),
            transparent_black(),
            Default::default(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_match_ranges_preserve_multiline_and_scrollback_boundaries() {
        let matches = vec![
            GridPoint::new(GridLine(-10), GridColumn(2))
                ..=GridPoint::new(GridLine(-8), GridColumn(4)),
            GridPoint::new(GridLine(-8), GridColumn(8))
                ..=GridPoint::new(GridLine(0), GridColumn(1)),
            GridPoint::new(GridLine(0), GridColumn(3))..=GridPoint::new(GridLine(0), GridColumn(6)),
            GridPoint::new(GridLine(3), GridColumn(0))..=GridPoint::new(GridLine(4), GridColumn(2)),
        ];
        for line in -12..7 {
            let expected: Vec<_> = matches
                .iter()
                .filter(|m| m.start().line.0 <= line && line <= m.end().line.0)
                .collect();
            let range = row_match_range(&matches, line);
            assert_eq!(
                matches[range].iter().collect::<Vec<_>>(),
                expected,
                "line {line}"
            );
        }
        assert_eq!(row_match_range(&[], 0), 0..0);
    }

    #[test]
    fn themed_panel_background_preserves_foregrounds_and_other_colors() {
        use alacritty_terminal::vte::ansi::Rgb;
        let palette = ColorPalette::builder()
            .background_override((15, 18, 22), (10, 33, 71))
            .build();
        let renderer = TerminalRenderer::new("monospace".into(), px(14.0), 1.2, palette.clone());
        let source = Color::Spec(Rgb {
            r: 15,
            g: 18,
            b: 22,
        });
        let unrelated = Color::Spec(Rgb {
            r: 40,
            g: 44,
            b: 52,
        });
        let mut panel = Cell::default();
        panel.c = 'a';
        panel.fg = source;
        panel.bg = source;
        let mut other = Cell::default();
        other.c = 'b';
        other.bg = unrelated;
        let (backgrounds, runs) = renderer.layout_row(
            0,
            vec![panel, other].into_iter().enumerate(),
            &Colors::default(),
        );
        assert_eq!(
            crate::colors::hsla_to_rgb(backgrounds[0].color),
            Rgb {
                r: 10,
                g: 33,
                b: 71
            }
        );
        assert_eq!(
            crate::colors::hsla_to_rgb(backgrounds[1].color),
            Rgb {
                r: 40,
                g: 44,
                b: 52
            }
        );
        assert_eq!(
            crate::colors::hsla_to_rgb(runs[0].fg_color),
            Rgb {
                r: 15,
                g: 18,
                b: 22
            }
        );
        let default = ColorPalette::default();
        let unchanged = default.resolve(source, &Colors::default());
        assert_eq!(default.map_background(unchanged), unchanged);
    }

    #[test]
    fn snapshot_runs_preserve_combining_wide_and_styled_cells() {
        let renderer =
            TerminalRenderer::new("monospace".into(), px(14.0), 1.2, ColorPalette::default());
        let colors = Colors::default();
        let mut accent = Cell::default();
        accent.c = 'e';
        accent.push_zerowidth('\u{301}');
        let mut wide = Cell::default();
        wide.c = '界';
        wide.flags = Flags::WIDE_CHAR;
        let mut spacer = Cell::default();
        spacer.flags = Flags::WIDE_CHAR_SPACER;
        let mut styled = Cell::default();
        styled.c = 'z';
        styled.flags = Flags::BOLD | Flags::ITALIC | Flags::UNDERLINE | Flags::STRIKEOUT;
        let cells = vec![accent, Cell::default(), wide, spacer, styled];
        let (_, runs) = renderer.layout_row(0, cells.into_iter().enumerate(), &colors);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "e\u{301} 界");
        assert_eq!(runs[0].char_count, 4);
        assert_eq!(runs[0].cell_count, 4);
        assert_eq!(runs[1].text, "z");
        assert_eq!(runs[1].start_col, 4);
        assert!(runs[1].bold && runs[1].italic && runs[1].underline && runs[1].strikethrough);
    }

    #[test]
    #[ignore = "measurement; run with --release --ignored --nocapture"]
    fn streaming_search_paint_index_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;
        let matches: Vec<Match> = (0..5_000)
            .map(|index| {
                let line = GridLine(index / 4 - 1_200);
                let col = GridColumn((index as usize % 4) * 10);
                GridPoint::new(line, col)..=GridPoint::new(line, GridColumn(col.0 + 5))
            })
            .collect();
        let mut baseline = Vec::with_capacity(101);
        let mut candidate = Vec::with_capacity(101);
        for _ in 0..101 {
            let start = Instant::now();
            let mut old_count = 0;
            for line in 0..48 {
                old_count += black_box(&matches)
                    .iter()
                    .filter(|m| m.start().line.0 <= line && line <= m.end().line.0)
                    .count();
            }
            baseline.push(start.elapsed().as_nanos());
            let start = Instant::now();
            let mut new_count = 0;
            for line in 0..48 {
                new_count += row_match_range(black_box(&matches), line).len();
            }
            candidate.push(start.elapsed().as_nanos());
            assert_eq!(black_box(old_count), black_box(new_count));
        }
        baseline.sort_unstable();
        candidate.sort_unstable();
        eprintln!(
            "search index 48 rows/5000 matches: baseline p50={}ns p95={}ns; candidate p50={}ns p95={}ns; baseline visited={} matches/frame",
            baseline[50],
            baseline[95],
            candidate[50],
            candidate[95],
            48 * matches.len()
        );
    }

    #[test]
    fn idle_renderer_release_preserves_metrics_and_clears_shared_scratch() {
        let renderer =
            TerminalRenderer::new("monospace".into(), px(14.0), 1.2, ColorPalette::default());
        let retained = renderer.clone();
        let metrics = (renderer.cell_width, renderer.cell_height);
        {
            let mut scratch = renderer.paint_scratch.lock();
            scratch.row_backgrounds.reserve(256);
            scratch.text_runs.reserve(256);
            scratch.block_rects.reserve(256);
            scratch.selection_rects.reserve(256);
            scratch.match_rects.reserve(256);
            scratch.decoration_rects.reserve(256);
        }
        renderer.release_idle_resources();
        renderer.release_idle_resources();
        assert_eq!((renderer.cell_width, renderer.cell_height), metrics);
        assert_eq!(renderer.font_family, "monospace");
        let scratch = retained.paint_scratch.lock();
        assert_eq!(scratch.row_backgrounds.capacity(), 0);
        assert_eq!(scratch.text_runs.capacity(), 0);
        assert_eq!(scratch.block_rects.capacity(), 0);
        assert_eq!(scratch.selection_rects.capacity(), 0);
        assert_eq!(scratch.match_rects.capacity(), 0);
        assert_eq!(scratch.decoration_rects.capacity(), 0);
    }

    #[test]
    fn test_renderer_creation() {
        let renderer = TerminalRenderer::new(
            "Fira Code".to_string(),
            px(14.0),
            1.2,
            ColorPalette::default(),
        );
        assert_eq!(renderer.font_family, "Fira Code");
        assert_eq!(renderer.font_size, px(14.0));
        assert_eq!(renderer.line_height_multiplier, 1.2);
    }

    #[test]
    fn test_background_rect_merge() {
        let black = Hsla::black();

        let rect1 = BackgroundRect::new(0, 5, 0, black);
        let rect2 = BackgroundRect::new(5, 10, 0, black);

        assert!(rect1.can_merge_with(&rect2));

        let rect3 = BackgroundRect::new(5, 10, 1, black);

        assert!(!rect1.can_merge_with(&rect3));
    }

    #[test]
    fn test_merge_backgrounds() {
        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.2,
            ColorPalette::default(),
        );
        let black = Hsla::black();

        let rects = vec![
            BackgroundRect::new(0, 5, 0, black),
            BackgroundRect::new(5, 10, 0, black),
        ];

        let merged = renderer.merge_backgrounds(rects);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_col, 0);
        assert_eq!(merged[0].end_col, 10);
    }

    #[test]
    fn test_merge_backgrounds_2d() {
        let black = Hsla::black();
        let rects = vec![
            BackgroundRect::new(0, 10, 0, black),
            BackgroundRect::new(0, 10, 1, black),
            BackgroundRect::new(0, 10, 2, black),
        ];

        let merged = TerminalRenderer::merge_backgrounds_2d(rects);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_col, 0);
        assert_eq!(merged[0].end_col, 10);
        assert_eq!(merged[0].row, 0);
        assert_eq!(merged[0].end_row, 3);
    }

    #[test]
    fn test_merge_backgrounds_2d_groups_interleaved_colors() {
        let red = gpui::hsla(0.0, 1.0, 0.5, 1.0);
        let blue = gpui::hsla(0.6, 1.0, 0.5, 1.0);
        let rects = vec![
            BackgroundRect::new(0, 10, 0, red),
            BackgroundRect::new(10, 20, 0, blue),
            BackgroundRect::new(10, 20, 1, blue),
            BackgroundRect::new(0, 10, 1, red),
        ];

        let merged = TerminalRenderer::merge_backgrounds_2d(rects);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|rect| rect.row == 0 && rect.end_row == 2));
        assert!(merged.iter().any(|rect| rect.color == red));
        assert!(merged.iter().any(|rect| rect.color == blue));
    }

    #[test]
    fn test_rasterize_box_and_block() {
        let origin = Point::default();
        let cell_w = px(10.0);
        let cell_h = px(20.0);
        let color = Hsla::black();

        let mut quads = Vec::new();
        assert!(rasterize_box_or_block(
            '█', 0, 0, color, origin, cell_w, cell_h, &mut quads
        ));
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].bounds.size.width, cell_w);
        assert_eq!(quads[0].bounds.size.height, cell_h);

        for (glyph, count) in [
            ('▀', 1), ('▄', 1), ('─', 1), ('│', 1), ('┌', 2), ('╬', 4), ('A', 0),
        ] {
            let before = quads.len();
            assert_eq!(
                rasterize_box_or_block(glyph, 1, 0, color, origin, cell_w, cell_h, &mut quads),
                count != 0
            );
            assert_eq!(quads.len(), before + count);
        }
    }
}
