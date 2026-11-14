//! Regression for the frame-scoped geometry handoff: a renderer polls its own
//! terminal's cell geometry and the components inside the frame must size
//! their images from that polled geometry, not from the process-wide cache.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rpi_tui::component::Component;
use rpi_tui::image::{Image, ImageOptions};
use rpi_tui::terminal::{Terminal, TerminalInfo};
use rpi_tui::tui_alt_screen::TuiAltScreen;
use rpi_tui::TUI;

/// A fake terminal that reports a settable pixel size for a fixed 80x8 grid.
/// Several clones can share one pixel state through `Arc`.
#[derive(Clone)]
struct PixelTerminal {
    pixels: Arc<std::sync::Mutex<(u16, u16)>>,
}

impl PixelTerminal {
    fn new(width: u16, height: u16) -> Self {
        Self {
            pixels: Arc::new(std::sync::Mutex::new((width, height))),
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
    fn start(
        &self,
        _: Box<dyn Fn(rpi_tui::terminal::InputEvent) + Send + Sync>,
        _: Box<dyn Fn() + Send + Sync>,
    ) {
    }
    fn stop(&self) {}
    fn is_tty(&self) -> bool {
        true
    }
    fn set_progress(&self, _: bool) {}
    fn flush(&self) {}
}

/// Counts how many rows the rendered image occupies.
struct ImageProbe {
    image: Arc<Image>,
    rows: Arc<AtomicUsize>,
}

impl Component for ImageProbe {
    fn render(&self, width: usize) -> Vec<String> {
        let lines = self.image.render(width);
        self.rows.store(lines.len(), Ordering::SeqCst);
        lines
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A PNG header is enough: only the pixel size is read.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut png = vec![0u8; 24];
    png[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    png[16..20].copy_from_slice(&width.to_be_bytes());
    png[20..24].copy_from_slice(&height.to_be_bytes());
    png
}

/// Frame content follows the geometry the renderer polls on the frame path.
///
/// The renderer must hand the geometry it polled to the components rendering
/// inside that frame. The process-wide cache is not polled by the renderer any
/// more, so without the handoff a font zoom (same grid, new pixel size) would
/// repaint the frame with image rows still derived from the old geometry.
#[test]
fn a_frame_rerenders_its_images_from_the_polled_geometry() {
    let _restore = CacheRestore;

    // The protocol lives in the global capability cache (it is detected from
    // the environment once); the cell geometry must come from the renderer's
    // own poll, which is what this test drives.
    let mut caps = rpi_tui::get_capabilities();
    caps.protocol = rpi_tui::ImageProtocol::Kitty;
    rpi_tui::set_capabilities(caps);

    // 800x160 pixels over an 80x8 grid: 10x20 pixels per cell.
    let terminal = PixelTerminal::new(800, 160);
    let image = Arc::new(Image::from_data(png(400, 200), ImageOptions::default()));
    let rows = Arc::new(AtomicUsize::new(0));
    let tui = TuiAltScreen::new(Box::new(terminal.clone()), false, None);
    tui.set_layout_root(Some(Arc::new(ImageProbe {
        image: image.clone(),
        rows: rows.clone(),
    })));
    tui.start_readerless();

    // First frame: 400x200 pixels over 10x20 cells is 40 columns wide / 10 rows.
    assert_eq!(
        rows.load(Ordering::SeqCst),
        10,
        "first frame at 10x20 cells"
    );

    // Font zoom: same 80x8 grid, cells become 10x10. The renderer must poll
    // that and the image rows must double.
    *terminal.pixels.lock().unwrap() = (800, 80);
    tui.ensure_size();
    tui.render_now(false);
    assert_eq!(
        rows.load(Ordering::SeqCst),
        20,
        "the image must follow the geometry the renderer polled, not the stale process-wide cache"
    );
}

/// Restores the capability cache the process started with.
struct CacheRestore;

impl Drop for CacheRestore {
    fn drop(&mut self) {
        rpi_tui::reset_capabilities_cache();
    }
}
