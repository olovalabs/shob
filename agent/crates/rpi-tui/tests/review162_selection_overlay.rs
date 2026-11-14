//! Probes from the #162 review (head `f599341`), reproduced verbatim from the
//! reviewer's comment. Two frame-geometry consumers were left outside the
//! frame scope that the implementation introduced:
//!
//! 1. `selection_text()` extracts against the process-wide cell geometry
//!    while the painted document was cached under the frame's geometry, so
//!    the mismatch returns `None` and a plain-text copy silently stops
//!    working after a cell-size change.
//! 2. `paint_overlays()` renders after the body frame's geometry scope has
//!    ended, so an overlay image keeps the old cell size while the body of the
//!    same frame already uses the new one.
//!
//! Both are offline renderer/selection API probes over a fake terminal: they
//! pin the geometry each consumer reads, not real terminal ioctl/font zoom.

use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use rpi_tui::component::Component;
use rpi_tui::image::{Image, ImageOptions};
use rpi_tui::scroll_view::{FollowMode, ScrollView, ScrollViewOptions, ScrollbarMode};
use rpi_tui::terminal::{InputEvent, Terminal, TerminalInfo};
use rpi_tui::tui_alt_screen::TuiAltScreen;
use rpi_tui::{get_capabilities, set_capabilities, ImageProtocol, TerminalCapabilities, TUI};

static LOCK: Mutex<()> = Mutex::new(());

struct Restore(TerminalCapabilities);
impl Drop for Restore {
    fn drop(&mut self) {
        set_capabilities(self.0.clone());
    }
}

fn setup() -> Restore {
    let saved = get_capabilities();
    let mut caps = saved.clone();
    caps.protocol = ImageProtocol::Kitty;
    caps.cell_width = 10;
    caps.cell_height = 20;
    set_capabilities(caps);
    Restore(saved)
}

#[derive(Clone)]
struct PixelTerminal {
    pixels: Arc<Mutex<(u16, u16)>>,
}

impl PixelTerminal {
    fn new() -> Self {
        Self {
            pixels: Arc::new(Mutex::new((800, 160))),
        }
    }
}

impl Terminal for PixelTerminal {
    fn info(&self) -> TerminalInfo {
        TerminalInfo {
            columns: 80,
            rows: 8,
            ..Default::default()
        }
    }
    fn write(&self, _: &str) {}
    fn hide_cursor(&self) {}
    fn show_cursor(&self) {}
    fn move_cursor(&self, _: usize, _: usize) {}
    fn clear_screen(&self) {}
    fn set_title(&self, _: &str) {}
    fn enable_mouse(&self) {}
    fn disable_mouse(&self) {}
    fn enter_raw_mode(&self) {}
    fn exit_raw_mode(&self) {}
    fn refresh_size(&self) {}
    fn window_size(&self) -> Option<crossterm::terminal::WindowSize> {
        let (width, height) = *self.pixels.lock().unwrap();
        Some(crossterm::terminal::WindowSize {
            columns: 80,
            rows: 8,
            width,
            height,
        })
    }
    fn start(&self, _: Box<dyn Fn(InputEvent) + Send + Sync>, _: Box<dyn Fn() + Send + Sync>) {}
    fn stop(&self) {}
    fn is_tty(&self) -> bool {
        true
    }
    fn set_progress(&self, _: bool) {}
    fn flush(&self) {}
}

struct Rows;
impl Component for Rows {
    fn render(&self, _: usize) -> Vec<String> {
        vec!["hello world".into(); 30]
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn drag(tui: &TuiAltScreen) -> Option<String> {
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), 0),
        (MouseEventKind::Drag(MouseButton::Left), 5),
        (MouseEventKind::Up(MouseButton::Left), 5),
    ] {
        tui.handle_selection_mouse_event(&MouseEvent {
            kind,
            column,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
    }
    assert!(tui.selection_dragged());
    tui.selection_text()
}

#[test]
fn selection_still_copies_after_frame_geometry_changes() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _restore = setup();
    let terminal = PixelTerminal::new();
    let scroll = Arc::new(ScrollView::new(
        Arc::new(Rows),
        ScrollViewOptions {
            follow: FollowMode::None,
            scrollbar: ScrollbarMode::Hidden,
            ..Default::default()
        },
    ));
    let tui = TuiAltScreen::new(Box::new(terminal.clone()), false, None);
    tui.set_layout_root(Some(scroll));
    tui.start_readerless();
    assert_eq!(
        drag(&tui),
        Some("hello".into()),
        "control before geometry change"
    );
    tui.clear_selection();
    *terminal.pixels.lock().unwrap() = (800, 80);
    tui.ensure_size();
    tui.render_now(false);
    assert_eq!(
        drag(&tui),
        Some("hello".into()),
        "plain text copy after geometry change"
    );
}

fn image() -> Arc<Image> {
    let mut png = vec![0u8; 24];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    png[16..20].copy_from_slice(&400u32.to_be_bytes());
    png[20..24].copy_from_slice(&200u32.to_be_bytes());
    Arc::new(Image::from_data(png, ImageOptions::default()))
}

struct ImageProbe {
    image: Arc<Image>,
    rows: Arc<AtomicUsize>,
}

impl Component for ImageProbe {
    fn render(&self, width: usize) -> Vec<String> {
        let rows = self.image.render(width);
        self.rows.store(rows.len(), Ordering::SeqCst);
        rows
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[test]
fn overlay_image_uses_the_same_geometry_as_body() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _restore = setup();
    let terminal = PixelTerminal::new();
    let body_rows = Arc::new(AtomicUsize::new(0));
    let overlay_rows = Arc::new(AtomicUsize::new(0));
    let tui = TuiAltScreen::new(Box::new(terminal.clone()), false, None);
    tui.set_layout_root(Some(Arc::new(ImageProbe {
        image: image(),
        rows: body_rows.clone(),
    })));
    tui.start_readerless();
    let _overlay = tui.show_overlay(
        Arc::new(ImageProbe {
            image: image(),
            rows: overlay_rows.clone(),
        }),
        None,
    );
    assert_eq!(body_rows.load(Ordering::SeqCst), 10);
    assert_eq!(overlay_rows.load(Ordering::SeqCst), 10);
    *terminal.pixels.lock().unwrap() = (800, 80);
    tui.ensure_size();
    tui.render_now(false);
    assert_eq!(body_rows.load(Ordering::SeqCst), 20);
    assert_eq!(
        overlay_rows.load(Ordering::SeqCst),
        20,
        "overlay must use frame geometry too"
    );
}
