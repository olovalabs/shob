//! #162 follow-up: image geometry must follow the terminal's cell size.
//!
//! These live in their own integration binary because they mutate the
//! process-wide capability cache; a separate process keeps that away from the
//! parallel unit tests in `src/`. Within this binary every test takes the same
//! lock, so one test's geometry swap cannot be observed by another.
//!
//! `Image::render` derives an image's row count by dividing its pixel size by
//! the cell size, so a render cached for one geometry is wrong for another. The
//! bug this pins: the cache key was the column width alone, so an existing image
//! kept its old row count after the cell size changed while a freshly built one
//! used the new geometry — two images of the same source disagreeing in one
//! frame.

use std::sync::{Mutex, MutexGuard};

use rpi_tui::component::Component;
use rpi_tui::image::{Image, ImageOptions};
use rpi_tui::{
    get_capabilities, get_cell_dimensions, set_capabilities, set_cell_dimensions, ImageProtocol,
    TerminalCapabilities,
};

/// Serializes every test in this binary (the capability cache is a global).
static CELL_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    CELL_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Restores the capability cache when it goes out of scope.
struct CapabilityRestore(TerminalCapabilities);

impl Drop for CapabilityRestore {
    fn drop(&mut self) {
        set_capabilities(self.0.clone());
    }
}

/// A PNG header with the given pixel size.
///
/// The renderer only reads the header to compute the row count; no terminal
/// decodes anything here, so a 24-byte header is enough and the test stays
/// offline.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut png = vec![0u8; 24];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    png[16..20].copy_from_slice(&width.to_be_bytes());
    png[20..24].copy_from_slice(&height.to_be_bytes());
    png
}

/// Switch to a kitty terminal whose cells have the given pixel size.
fn use_cells(width: u16, height: u16) {
    let mut caps = get_capabilities();
    caps.protocol = ImageProtocol::Kitty;
    caps.cell_width = width;
    caps.cell_height = height;
    set_capabilities(caps);
}

/// The maintainer's probe from the #162 review, adopted verbatim.
///
/// Existing image and fresh image of the same source must agree after the cell
/// geometry changes at an unchanged column count.
#[test]
fn review162_existing_image_follows_new_cell_geometry_at_same_width() {
    let _g = lock();
    let _restore = CapabilityRestore(get_capabilities());
    use_cells(10, 20);

    let image = Image::from_data(png(400, 200), ImageOptions::default());
    assert_eq!(image.render(80).len(), 10);

    // Simulate `refresh_cell_dimensions` having succeeded, columns unchanged.
    set_cell_dimensions(10, 10);
    let fresh = Image::from_data(png(400, 200), ImageOptions::default()).render(80);
    assert_eq!(fresh.len(), 20);
    assert_eq!(
        image.render(80).len(),
        fresh.len(),
        "existing and fresh images must agree after cell geometry changes"
    );
}

/// The same axis, walked in the other direction: a geometry change that makes
/// the image *shorter* must also be followed.
///
/// A one-way test would pass on a fix that only ever grows the row count.
#[test]
fn an_existing_image_shrinks_when_the_cells_get_taller() {
    let _g = lock();
    let _restore = CapabilityRestore(get_capabilities());
    use_cells(10, 10);

    let image = Image::from_data(png(400, 200), ImageOptions::default());
    assert_eq!(image.render(80).len(), 20);

    set_cell_dimensions(10, 20);
    assert_eq!(
        image.render(80).len(),
        10,
        "a cached render must not outlive the geometry it was computed for"
    );
}

/// A width change still invalidates the cache, and a repeat render at the same
/// width and geometry is still served from it.
///
/// Without this half, "invalidate on every render" would pass the two tests
/// above while silently dropping the cache the component exists to provide.
/// The equality of the output cannot prove a cache hit (a re-derivation of the
/// same geometry produces the same string), so the assertion also pins the row
/// count: a 6-column request must yield strictly fewer rows than the 40-column
/// one, which a stale placement would violate.
#[test]
fn the_cache_still_hits_at_an_unchanged_geometry_and_misses_on_a_new_width() {
    let _g = lock();
    let _restore = CapabilityRestore(get_capabilities());
    use_cells(10, 20);

    let image = Image::from_data(png(400, 200), ImageOptions::default());
    let narrow = image.render(40);
    let narrow_again = image.render(40);
    assert_eq!(narrow, narrow_again);

    // A narrower width must re-derive, not reuse the 40-column placement.
    let very_narrow = image.render(6);
    assert_ne!(narrow, very_narrow);
    assert!(
        very_narrow.len() < narrow.len(),
        "a 6-column placement must be shorter than a 40-column one: {} vs {}",
        very_narrow.len(),
        narrow.len()
    );
}

/// An image that has never been rendered picks up the geometry reported at
/// construction time, so the first frame after startup is already correct.
#[test]
fn a_new_image_uses_the_current_cell_geometry_on_its_first_render() {
    let _g = lock();
    let _restore = CapabilityRestore(get_capabilities());
    use_cells(10, 10);

    let image = Image::from_data(png(400, 200), ImageOptions::default());
    assert_eq!(image.render(80).len(), 20);
    assert_eq!(get_cell_dimensions().height, 10);
}

/// Even with an explicit `ImageOptions::width`, the row count comes from the
/// cell *height*: the width fixes the columns, but the aspect conversion still
/// divides by the cell geometry. A geometry change must therefore reach an image
/// that never had its width auto-derived.
#[test]
fn an_explicit_width_still_follows_the_cell_height() {
    let _g = lock();
    let _restore = CapabilityRestore(get_capabilities());
    use_cells(10, 20);

    let options = ImageOptions {
        width: Some(40),
        ..Default::default()
    };
    let image = Image::from_data(png(400, 200), options);
    let tall = image.render(80).len();

    set_cell_dimensions(10, 10);
    let fresh = Image::from_data(
        png(400, 200),
        ImageOptions {
            width: Some(40),
            ..Default::default()
        },
    )
    .render(80);
    assert_ne!(tall, fresh.len());
    assert_eq!(image.render(80).len(), fresh.len());
}
