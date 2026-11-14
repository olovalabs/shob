//! Kitty placements must be cropped to the rows that are actually on screen.
//!
//! A Kitty placement is anchored to a screen cell: the terminal paints `r`
//! rows downward from the cursor and knows nothing about the TUI's clip
//! rectangles. The fullscreen renderer therefore has to crop the placement
//! itself, or two things go wrong:
//!
//! - an image near the end of the viewport paints over the dock below it
//!   (placements are drawn above text by default), and
//! - an image whose first row scrolled above the viewport disappears entirely,
//!   because the escape lives on that row and the visible slice drops it.
//!
//! Both are asserted here on the rendered frame, which is what the terminal
//! receives.

use std::any::Any;
use std::sync::Arc;

use rpi_tui::component::Component;
use rpi_tui::layout::{render_layout_frame, LayoutFrame};
use rpi_tui::scroll_view::{FollowMode, ScrollView, ScrollViewOptions};
use rpi_tui::spacer::Spacer;
use rpi_tui::terminal_image::{register_kitty_image, KittyImageMetadata};
use rpi_tui::vstack::{StackEntryOptions, VStack};

const IMAGE_ID: u32 = 9_101;
const IMAGE_ROWS: u16 = 8;
const IMAGE_HEIGHT_PX: u32 = 800;
const IMAGE_ROW: usize = 6;
const VIEWPORT_ROWS: usize = 10;

/// A fixed document: one string per row.
struct Rows(Vec<String>);

impl Component for Rows {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.clone()
    }

    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn register() {
    register_kitty_image(KittyImageMetadata {
        id: IMAGE_ID,
        columns: 20,
        rows: IMAGE_ROWS,
        width_px: 400,
        height_px: IMAGE_HEIGHT_PX,
    });
}

/// A placement exactly as `Image::render` emits it, without the payload.
fn image_line() -> String {
    format!("\x1b_Ga=T,f=100,C=1,q=2,i={IMAGE_ID},c=20,r={IMAGE_ROWS},m=0;AAAA\x1b\\")
}

/// A document of `total` rows with the image block starting at `image_row`.
fn document(image_row: usize, total: usize) -> Arc<Rows> {
    let mut lines: Vec<String> = (0..total).map(|row| format!("row {row}")).collect();
    lines[image_row] = image_line();
    for offset in 1..usize::from(IMAGE_ROWS) {
        lines[image_row + offset] = String::new();
    }
    Arc::new(Rows(lines))
}

fn scroll_view(document: Arc<Rows>) -> Arc<ScrollView> {
    Arc::new(ScrollView::new(
        document,
        ScrollViewOptions {
            follow: FollowMode::None,
            ..Default::default()
        },
    ))
}

/// Transcript viewport plus a four-row dock underneath it.
fn frame_with_dock(scroll: Arc<ScrollView>) -> LayoutFrame {
    let stack = VStack::new();
    stack.add_child_with_options(scroll, StackEntryOptions::with_basis(VIEWPORT_ROWS).grow(1));
    stack.add_child_with_options(Arc::new(Spacer::new(4)), StackEntryOptions::with_basis(4));
    render_layout_frame(Arc::new(stack), 80, VIEWPORT_ROWS + 4)
}

#[test]
fn a_placement_cannot_paint_below_the_viewport() {
    register();
    // The image starts at row 6 and covers 8 rows, but only rows 6..9 fit in
    // the viewport. Without the crop the terminal would paint four extra rows
    // over the dock.
    let frame = frame_with_dock(scroll_view(document(IMAGE_ROW, 40)));
    let line = &frame.lines[IMAGE_ROW];
    assert!(
        line.contains(",y=0,h=400,r=4;"),
        "the placement must be cropped to the four rows above the dock, got {line:?}"
    );
    assert!(
        frame.lines[VIEWPORT_ROWS..]
            .iter()
            .all(|row| !row.contains("\x1b_G")),
        "no placement may be painted into the dock rows"
    );
}

#[test]
fn a_fully_visible_placement_is_not_rewritten() {
    register();
    let frame = frame_with_dock(scroll_view(document(0, 40)));
    let line = &frame.lines[0];
    assert!(
        line.contains(&format!(",r={IMAGE_ROWS},m=0;")),
        "an image that fits keeps its rendered height, got {line:?}"
    );
    assert!(
        !line.contains(",y="),
        "an image that fits needs no source crop, got {line:?}"
    );
}

#[test]
fn a_scrolled_placement_is_reanchored_to_the_first_visible_row() {
    register();
    let scroll = scroll_view(document(5, 40));
    // First pass establishes the content height the scroll position is
    // clamped against.
    let _ = render_layout_frame(scroll.clone(), 80, VIEWPORT_ROWS);
    // Scroll three rows into the image: rows 0..3 of it are hidden.
    scroll.scroll_to(8);
    let frame = frame_with_dock(scroll);

    let top = &frame.lines[0];
    assert!(
        top.contains("\x1b_G"),
        "the placement must be re-anchored to the top row, got {top:?}"
    );
    assert!(
        top.contains(",y=300,h=500,r=5;"),
        "the placement must show source rows 3..8 over five rows, got {top:?}"
    );
    assert!(
        frame.lines[1..].iter().all(|row| !row.contains("\x1b_G")),
        "the placement must be emitted once, on the row it is anchored to"
    );
}
