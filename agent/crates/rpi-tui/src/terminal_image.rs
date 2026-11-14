//! Terminal image support.
//!
//! Provides support for displaying images in terminals using Kitty and iTerm2 protocols.

use std::collections::HashMap;
use std::sync::Mutex;

/// Image protocol type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    /// Kitty graphics protocol.
    Kitty,
    /// iTerm2 inline images.
    ITerm2,
    /// No image support.
    None,
}

/// Terminal capabilities for image display.
#[derive(Debug, Clone)]
pub struct TerminalCapabilities {
    /// Supported image protocol.
    pub protocol: ImageProtocol,
    /// Cell dimensions in pixels.
    pub cell_width: u16,
    pub cell_height: u16,
    /// Screen dimensions in cells.
    pub columns: u16,
    pub rows: u16,
}

impl Default for TerminalCapabilities {
    fn default() -> Self {
        Self {
            protocol: ImageProtocol::None,
            cell_width: 8,
            cell_height: 16,
            columns: 80,
            rows: 24,
        }
    }
}

/// Image dimensions.
#[derive(Debug, Clone, Copy)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

/// Cell dimensions in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDimensions {
    pub width: u16,
    pub height: u16,
}

/// Options for rendering an image.
#[derive(Debug, Clone)]
pub struct ImageRenderOptions {
    /// Width in cells (None = auto).
    pub width: Option<u16>,
    /// Height in cells (None = auto).
    pub height: Option<u16>,
    /// Whether to preserve aspect ratio.
    pub preserve_aspect: bool,
    /// X offset in cells.
    pub x: u16,
    /// Y offset in cells.
    pub y: u16,
    /// Z-index for overlapping images (Kitty only).
    pub z_index: Option<i32>,
    /// Image ID for manipulation (Kitty only).
    pub id: Option<u32>,
}

impl Default for ImageRenderOptions {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            preserve_aspect: true,
            x: 0,
            y: 0,
            z_index: None,
            id: None,
        }
    }
}

/// Global capabilities cache.
static CAPABILITIES_CACHE: std::sync::OnceLock<Mutex<TerminalCapabilities>> =
    std::sync::OnceLock::new();

/// Get cached capabilities.
pub fn get_capabilities() -> TerminalCapabilities {
    let cache = CAPABILITIES_CACHE.get_or_init(|| Mutex::new(detect_capabilities()));
    cache.lock().map(|c| c.clone()).unwrap_or_default()
}

/// Set capabilities.
pub fn set_capabilities(capabilities: TerminalCapabilities) {
    let cache = CAPABILITIES_CACHE.get_or_init(|| Mutex::new(detect_capabilities()));
    if let Ok(mut c) = cache.lock() {
        *c = capabilities;
    }
}

/// Reset capabilities cache.
pub fn reset_capabilities_cache() {
    set_capabilities(TerminalCapabilities::default());
}

/// Detect terminal capabilities.
/// Detect supported protocols from the terminal environment, as Pi does.
pub fn detect_capabilities() -> TerminalCapabilities {
    let mut caps = TerminalCapabilities::default();

    caps.protocol = detect_image_protocol(|key| std::env::var(key).ok());

    // Get terminal size
    if let Ok(size) = crossterm::terminal::size() {
        caps.columns = size.0;
        caps.rows = size.1;
    }

    // Cell size in pixels, when the platform reports it.
    if let Some(cell) = query_cell_dimensions(
        crossterm::terminal::window_size().ok(),
        crossterm::terminal::size().ok(),
    ) {
        caps.cell_width = cell.width;
        caps.cell_height = cell.height;
    }

    caps
}

/// Default cell size used when the terminal cannot report one.
///
/// Matches the historical [`TerminalCapabilities::default`] values so a
/// terminal that reports nothing keeps rendering exactly as before.
pub const DEFAULT_CELL_WIDTH: u16 = 8;
pub const DEFAULT_CELL_HEIGHT: u16 = 16;

/// Derive the cell size in pixels from a window size report.
///
/// `crossterm::terminal::window_size` returns the terminal's pixel size on
/// Unix but is `Unsupported` on Windows, and even on Unix some terminals report
/// zero pixels. Both cases fall back to [`DEFAULT_CELL_WIDTH`] /
/// [`DEFAULT_CELL_HEIGHT`] instead of guessing a cell count from nothing.
/// A reported pixel size that is smaller than the cell grid is treated as
/// unusable for the same reason: it would imply a sub-pixel cell.
pub fn query_cell_dimensions(
    window: Option<crossterm::terminal::WindowSize>,
    size: Option<(u16, u16)>,
) -> Option<CellDimensions> {
    let window = window?;
    let (columns, rows) = match size {
        Some((c, r)) if c > 0 && r > 0 => (c, r),
        _ => (window.columns, window.rows),
    };
    if columns == 0 || rows == 0 {
        return None;
    }
    if u32::from(window.width) < u32::from(columns) || u32::from(window.height) < u32::from(rows) {
        return None;
    }
    let width = window.width / columns;
    let height = window.height / rows;
    if width == 0 || height == 0 {
        return None;
    }
    Some(CellDimensions { width, height })
}

/// Serializes tests that mutate the process-wide capability cache.
///
/// The cache is a `static`, so a test that swaps the cell geometry is visible
/// to every other test in the same binary. Geometry-sensitive assertions in
/// `image.rs` and `scroll_view.rs` take this lock so one test's swap cannot
/// invalidate another test's cached render mid-assertion.
///
/// Renderers no longer read the geometry through this cache: [`crate::tui_alt_screen::TuiAltScreen`]
/// polls its own terminal and keeps the result on the instance, which is what
/// isolates the renderer tests without any shared lock. This lock remains only
/// for the component-level probes that deliberately drive `Image` and
/// `ScrollView` through the cache.
#[cfg(test)]
pub(crate) fn capability_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Refresh the cached cell size after a resize.
///
/// Callers that learn about a resize (`Event::Resize`, the renderer's own size
/// check, or the size watchdog) call this so image sizing follows the new cell
/// geometry. It is a no-op on terminals that do not report pixel sizes, leaving
/// the previously cached (or default) values in place.
///
/// A character grid can stay the same while the pixel size of a cell changes
/// (font zoom, moving the window to a monitor with another DPI), so callers
/// cannot infer a geometry change from columns and rows: they compare
/// [`get_cell_dimensions`] against what they last painted.
pub fn refresh_cell_dimensions() {
    if let Some(cell) = query_cell_dimensions(
        crossterm::terminal::window_size().ok(),
        crossterm::terminal::size().ok(),
    ) {
        set_cell_dimensions(cell.width, cell.height);
    }
}

fn detect_image_protocol(env: impl Fn(&str) -> Option<String>) -> ImageProtocol {
    let program = env("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let term = env("TERM").unwrap_or_default().to_lowercase();
    let present = |key| env(key).is_some_and(|value| !value.is_empty());
    if present("TMUX") || term.starts_with("tmux") || term.starts_with("screen") {
        return ImageProtocol::None;
    }
    if present("KITTY_WINDOW_ID")
        || program == "kitty"
        || program == "ghostty"
        || term.contains("ghostty")
        || present("GHOSTTY_RESOURCES_DIR")
        || program == "wezterm"
        || present("WEZTERM_PANE")
        || program == "warpterminal"
        || present("WARP_SESSION_ID")
        || present("WARP_TERMINAL_SESSION_UUID")
    {
        ImageProtocol::Kitty
    } else if present("ITERM_SESSION_ID") || program == "iterm.app" {
        ImageProtocol::ITerm2
    } else {
        ImageProtocol::None
    }
}

/// Set cell dimensions.
pub fn set_cell_dimensions(width: u16, height: u16) {
    let mut caps = get_capabilities();
    caps.cell_width = width;
    caps.cell_height = height;
    set_capabilities(caps);
}

/// Get cell dimensions.
pub fn get_cell_dimensions() -> CellDimensions {
    let caps = get_capabilities();
    CellDimensions {
        width: caps.cell_width,
        height: caps.cell_height,
    }
}

thread_local! {
    /// The cell geometry the renderer that is painting the current frame polled
    /// from its own terminal.
    ///
    /// A renderer polls the pixel size per frame and lays the frame out for it;
    /// components inside the frame (`Image`, `ScrollView`'s cached documents)
    /// derive image rows from the same geometry, so they must read the renderer's
    /// polled value rather than a process-wide cache another terminal or test can
    /// move mid-frame. The renderer sets this for the duration of a layout pass;
    /// outside one, the process-wide cache (last refreshed by
    /// [`refresh_cell_dimensions`]) still answers.
    static FRAME_CELL_GEOMETRY: std::cell::Cell<Option<CellDimensions>> =
        const { std::cell::Cell::new(None) };
}

/// Run `f` with components reading `geometry` for their cell size.
///
/// The renderer wraps its layout pass in this so image sizing follows the
/// geometry the renderer itself polled, on the thread doing the layout.
pub(crate) fn with_frame_cell_geometry<T>(geometry: CellDimensions, f: impl FnOnce() -> T) -> T {
    let previous = FRAME_CELL_GEOMETRY.with(|slot| slot.replace(Some(geometry)));
    let result = f();
    FRAME_CELL_GEOMETRY.with(|slot| slot.set(previous));
    result
}

/// The cell size components should size images with right now.
///
/// The frame-scoped geometry wins because it is what the frame being painted
/// was polled for; the process-wide cache is the fallback outside a frame.
pub(crate) fn effective_cell_dimensions() -> CellDimensions {
    FRAME_CELL_GEOMETRY
        .with(|slot| slot.get())
        .unwrap_or_else(get_cell_dimensions)
}

/// Allocate a unique image ID.
pub fn allocate_image_id() -> u32 {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// Placement id used for every image this client displays.
///
/// A placement is identified by the `(i=, p=)` pair, so one constant id is
/// enough: re-placing the same image at a new cell replaces its placement,
/// which the protocol defines as the way to move an image "without flicker".
/// Without a placement id every `a=p` would create an additional placement.
const KITTY_PLACEMENT_ID: u32 = 1;

/// Encode image data for Kitty protocol.
pub fn encode_kitty(data: &[u8], options: &ImageRenderOptions) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let mut parts = Vec::new();

    // Build control string
    let mut control = String::new();
    control.push_str("a=T,f=100,C=1,q=2"); // PNG; keep cursor fixed, suppress responses

    if let Some(id) = options.id {
        control.push_str(&format!(",i={id},p={KITTY_PLACEMENT_ID}"));
    }

    if let Some(z) = options.z_index {
        control.push_str(&format!(",z={}", z));
    }

    // Calculate dimensions
    if let (Some(w), Some(h)) = (options.width, options.height) {
        control.push_str(&format!(",c={w},r={h}"));
    }

    // Encode image data as base64
    let encoded = STANDARD.encode(data);

    // Chunk the data (Kitty has a 4096 byte limit per chunk)
    let chunk_size = 4000;
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    // Kitty caps one escape at 4096 bytes, so larger payloads must be split.
    // The first chunk carries the control keys plus `m=1`; each following chunk
    // carries `m=1` alone, and the final one closes the transfer with `m=0`.
    // Without that flag a terminal decodes every chunk as a complete image, so
    // a multi-chunk payload is read as truncated image data and dropped.
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            parts.push(format!("\x1b_G{},m={more};{chunk}\x1b\\", control));
        } else {
            parts.push(format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }

    parts.join("")
}

/// Encode image data for iTerm2 protocol.
pub fn encode_iterm2(data: &[u8], options: &ImageRenderOptions) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let encoded = STANDARD.encode(data);

    let mut name = String::new();
    if let Some(id) = options.id {
        name = format!("name=id{}", id);
    }

    let mut dims = String::new();
    if let Some(w) = options.width {
        dims = format!("width={}", w);
    }
    if let Some(h) = options.height {
        if !dims.is_empty() {
            dims.push(';');
        }
        dims.push_str(&format!("height={}", h));
    }

    format!("\x1b]1337;File={};{}:{}\x07", name, dims, encoded)
}

/// Delete a Kitty image by ID.
pub fn delete_kitty_image(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={}\x1b\\", id)
}

/// Delete all Kitty images.
pub fn delete_all_kitty_images() -> String {
    "\x1b_Ga=d,d=A\x1b\\".to_string()
}

/// Delete this client's placement of a Kitty image, keeping its pixel data.
///
/// Lowercase `d=i` only drops the placement, and the `p=` key scopes it to the
/// `(image id, placement id)` pair this client creates. The stored data stays
/// available, so the same id can be placed again without re-transmitting it.
pub fn delete_kitty_placement(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={id},p={KITTY_PLACEMENT_ID}\x1b\\")
}

/// Delete the placements whose retained line changed or disappeared.
///
/// Only the placement is deleted, never the image data: a row that is rewritten
/// to a placement-only `a=p` needs the payload to still be in the terminal.
/// An image that is still on screen in the new frame is not deleted at all —
/// re-placing it with the same `(i=, p=)` pair moves its placement, and the
/// terminal replaces the old one instead of leaving a stale copy behind.
pub(crate) fn removed_kitty_placements(previous: &[String], current: &[String]) -> String {
    let live: Vec<u32> = current
        .iter()
        .filter_map(|line| kitty_transmission_id(line))
        .collect();
    let mut output = String::new();
    for (row, line) in previous.iter().enumerate() {
        if current.get(row) == Some(line) {
            continue;
        }
        for chunk in line.split("\x1b_G").skip(1) {
            let control = chunk.split(';').next().unwrap_or_default();
            if !control.split(',').any(|field| field == "a=T") {
                continue;
            }
            if let Some(id) = control
                .split(',')
                .find_map(|field| field.strip_prefix("i=")?.parse::<u32>().ok())
            {
                if live.contains(&id) {
                    continue;
                }
                output.push_str(&delete_kitty_placement(id));
            }
        }
    }
    output
}

/// Geometry of a Kitty placement, registered by the component that rendered it.
///
/// A Kitty placement is anchored to a screen cell: the terminal paints `rows`
/// rows downward from the cursor and knows nothing about the TUI's clip
/// rectangles. The renderer therefore has to crop a placement itself whenever
/// only part of it fits on screen — both at the bottom (an image at the end of
/// the viewport would otherwise paint over the dock below it) and at the top
/// (the escape lives on the image's first row, so slicing the viewport would
/// drop a partially scrolled image entirely). The source rectangle is expressed
/// in pixels, which is why the pixel dimensions are tracked as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyImageMetadata {
    /// Kitty image id (`i=`).
    pub id: u32,
    /// Placement width in cells (`c=`).
    pub columns: u16,
    /// Placement height in cells (`r=`).
    pub rows: u16,
    /// Source image width in pixels.
    pub width_px: u32,
    /// Source image height in pixels.
    pub height_px: u32,
}

/// Registered placements, keyed by image id.
///
/// Image ids are unique for the whole process (`allocate_image_id`) and a
/// placement's geometry never changes once rendered, so an entry stays valid
/// for as long as any rendered document can retain its row. The map is
/// deliberately not evicted: dropping an entry would silently disable both the
/// bottom crop and the top re-anchor for an image the user scrolls back to,
/// and an entry is a handful of words. A long session that shows thousands of
/// images pays a few hundred kilobytes, not the correctness of its transcript.
static KITTY_IMAGES: std::sync::OnceLock<Mutex<HashMap<u32, KittyImageMetadata>>> =
    std::sync::OnceLock::new();

fn kitty_images() -> &'static Mutex<HashMap<u32, KittyImageMetadata>> {
    KITTY_IMAGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Tallest registered placement, used to bound the backwards search for an
/// image row above the viewport.
static MAX_KITTY_IMAGE_ROWS: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// Record the geometry of a placement emitted by [`encode_kitty`].
///
/// Called by the image component when it renders; the layout consults the
/// registry to crop placements to the visible rows.
pub fn register_kitty_image(metadata: KittyImageMetadata) {
    if metadata.id == 0 || metadata.rows == 0 || metadata.height_px == 0 {
        return;
    }
    MAX_KITTY_IMAGE_ROWS.fetch_max(metadata.rows, std::sync::atomic::Ordering::Relaxed);
    let Ok(mut images) = kitty_images().lock() else {
        return;
    };
    images.insert(metadata.id, metadata);
}

/// Tallest placement registered so far, in cells.
pub fn max_kitty_image_rows() -> usize {
    MAX_KITTY_IMAGE_ROWS.load(std::sync::atomic::Ordering::Relaxed) as usize
}

/// Whether a rendered row carries inline image data (Kitty or iTerm2).
pub fn is_image_line(line: &str) -> bool {
    line.contains("\x1b_G") || line.contains("\x1b]1337;File=")
}

/// Control string of the first Kitty escape on `line` (`a=T,...,i=1`).
fn kitty_control(line: &str) -> Option<&str> {
    let start = line.find("\x1b_G")? + 3;
    let end = start + line[start..].find(';')?;
    Some(&line[start..end])
}

/// Metadata of the placement carried by `line`, if it is a registered image.
pub fn kitty_image_metadata(line: &str) -> Option<KittyImageMetadata> {
    let control = kitty_control(line)?;
    let id = control
        .split(',')
        .find_map(|field| field.strip_prefix("i=")?.parse::<u32>().ok())?;
    kitty_images().lock().ok()?.get(&id).copied()
}

/// Source rectangle (in pixels) covering `visible` of a placement's `rows`,
/// skipping the first `hidden` rows.
fn crop_source_rect(metadata: &KittyImageMetadata, hidden: u16, visible: u16) -> (u32, u32) {
    let rows = u64::from(metadata.rows.max(1));
    let height = u64::from(metadata.height_px.max(1));
    let hidden = u64::from(hidden);
    let visible = u64::from(visible);
    let top = height * hidden / rows;
    let bottom = (height * (hidden + visible)).div_ceil(rows).min(height);
    (top as u32, bottom.saturating_sub(top).max(1) as u32)
}

/// Crop a Kitty placement to the rows that are actually on screen.
///
/// `hidden_rows` rows at the top of the placement are cut (the image scrolled
/// above the viewport), `visible_rows` rows are kept. Kitty has no notion of a
/// partial placement, so the crop is expressed as a source rectangle (`y`/`h`,
/// in pixels) displayed over `r` rows. Lines that are not registered Kitty
/// placements, or that are already showing the requested rows, are returned
/// unchanged; a placement with no visible rows left is dropped entirely.
pub fn crop_kitty_image_line(line: &str, hidden_rows: u16, visible_rows: u16) -> String {
    let Some(metadata) = kitty_image_metadata(line) else {
        return line.to_string();
    };
    let hidden = hidden_rows.min(metadata.rows);
    if hidden >= metadata.rows || visible_rows == 0 {
        return String::new();
    }
    let visible = visible_rows.min(metadata.rows - hidden);
    if hidden == 0 && visible == metadata.rows {
        return line.to_string();
    }
    let Some(escape) = line.find("\x1b_G") else {
        return line.to_string();
    };
    let control_start = escape + 3;
    let Some(control_len) = line[control_start..].find(';') else {
        return line.to_string();
    };
    let control_end = control_start + control_len;
    // Drop any previous crop: it describes the placement as it was rendered,
    // not the rows this frame can show.
    let mut controls: Vec<&str> = line[control_start..control_end]
        .split(',')
        .filter(|field| {
            !field.starts_with("y=") && !field.starts_with("h=") && !field.starts_with("r=")
        })
        .collect();
    let (source_y, source_height) = crop_source_rect(&metadata, hidden, visible);
    let crop = format!("y={source_y},h={source_height},r={visible}");
    controls.push(&crop);
    format!(
        "{}\x1b_G{};{}",
        &line[..escape],
        controls.join(","),
        &line[control_end + 1..]
    )
}

/// Per-terminal record of the image payloads the terminal is known to hold.
///
/// A placement cannot be moved without re-emitting it, so every frame that
/// repaints an image row has to re-emit the placement. Re-sending the payload
/// (hundreds of kilobytes for a screenshot) on every scroll step is what makes
/// scrolling crawl, so a row whose data is already in the terminal is rewritten
/// to a placement-only `a=p` command.
///
/// The terminal's image store belongs to one terminal connection, so this state
/// lives on the screen that owns that terminal, never in a process global. It
/// is a hint that may only ever err towards re-sending:
///
/// * an id is trusted only while the image was on screen in the *previous*
///   frame — an image that scrolled away is uploaded again when it comes back,
///   so a terminal that dropped the data meanwhile cannot show a blank;
/// * every event that can drop the store (`\x1b[2J`, a resize, entering the
///   alternate screen) calls [`KittyUploads::invalidate`];
/// * forgetting an id costs one payload, claiming one the terminal no longer
///   has would cost the image.
#[derive(Debug, Default)]
pub struct KittyUploads {
    /// Ids painted in the previous frame.
    painted: Vec<u32>,
    /// Ids painted in the frame being built.
    next: Vec<u32>,
}

impl KittyUploads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the terminal is known to hold this image's pixel data.
    pub fn is_uploaded(&self, id: u32) -> bool {
        self.painted.contains(&id)
    }

    /// Record that the image is on screen in the frame being built.
    pub fn note_painted(&mut self, id: u32) {
        if !self.next.contains(&id) {
            self.next.push(id);
        }
    }

    /// Close the frame: what was painted becomes what is trusted next time.
    pub fn end_frame(&mut self) {
        std::mem::swap(&mut self.painted, &mut self.next);
        self.next.clear();
    }

    /// Forget every upload, e.g. after `\x1b[2J` cleared the screen's images.
    pub fn invalidate(&mut self) {
        self.painted.clear();
        self.next.clear();
    }
}

/// Image id of a row that carries a full Kitty transmission (`a=T`).
pub fn kitty_transmission_id(line: &str) -> Option<u32> {
    let control = kitty_control(line)?;
    if !control.split(',').any(|field| field == "a=T") {
        return None;
    }
    control
        .split(',')
        .find_map(|field| field.strip_prefix("i=")?.parse::<u32>().ok())
}

/// Control keys that describe a placement rather than the pixel data.
const KITTY_PLACEMENT_KEYS: [&str; 16] = [
    "i", "p", "x", "y", "w", "h", "X", "Y", "c", "r", "C", "U", "z", "P", "Q", "H",
];

/// Rewrite a transmitted image row into a placement-only command.
///
/// Returns `None` when the row carries no Kitty transmission or the terminal is
/// not known to hold its data, in which case the row has to be written as is.
pub fn kitty_placement_line(line: &str, uploads: &KittyUploads) -> Option<String> {
    let escape = line.find("\x1b_G")?;
    let control_start = escape + 3;
    let control_end = control_start + line[control_start..].find(';')?;
    let control = &line[control_start..control_end];
    let fields: Vec<&str> = control.split(',').collect();
    if !fields.contains(&"a=T") {
        return None;
    }
    let id = fields
        .iter()
        .find_map(|field| field.strip_prefix("i=")?.parse::<u32>().ok())?;
    if !uploads.is_uploaded(id) {
        return None;
    }

    // Walk the chunk sequence: a chunk whose control says `m=1` is followed by
    // another escape, and the payload ends after the first `m=0` chunk.
    let mut chunk_start = control_start;
    let mut more = fields.contains(&"m=1");
    let end = loop {
        let chunk_control_end = chunk_start + line[chunk_start..].find(';')?;
        let payload_start = chunk_control_end + 1;
        let terminator = payload_start + line[payload_start..].find("\x1b\\")?;
        if !more {
            break terminator + 2;
        }
        let next = terminator + 2;
        if !line[next..].starts_with("\x1b_G") {
            return None;
        }
        chunk_start = next + 3;
        let next_control_end = chunk_start + line[chunk_start..].find(';')?;
        more = line[chunk_start..next_control_end]
            .split(',')
            .any(|field| field == "m=1");
    };

    let keys: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|field| {
            field
                .split('=')
                .next()
                .is_some_and(|key| KITTY_PLACEMENT_KEYS.contains(&key))
        })
        .collect();
    Some(format!(
        "{}\x1b_Ga=p,q=2,{}\x1b\\{}",
        &line[..escape],
        keys.join(","),
        &line[end..]
    ))
}

/// Create an OSC 8 hyperlink.
pub fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{}\x07{}\x1b]8;;\x07", url, text)
}

/// Image dimensions for common formats.

/// Get PNG dimensions from header.
pub fn get_png_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    if data.len() < 24 {
        return None;
    }

    // PNG signature: 89 50 4E 47 0D 0A 1A 0A
    if data[0..8] != [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return None;
    }

    // Width at offset 16 (4 bytes, big endian)
    let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    // Height at offset 20 (4 bytes, big endian)
    let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);

    Some(ImageDimensions { width, height })
}

/// Get JPEG dimensions from header.
pub fn get_jpeg_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }

    let mut pos = 2;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }

        let marker = data[pos + 1];

        // SOF markers (Start of Frame)
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            if pos + 9 > data.len() {
                return None;
            }
            let height = u16::from_be_bytes([data[pos + 5], data[pos + 6]]) as u32;
            let width = u16::from_be_bytes([data[pos + 7], data[pos + 8]]) as u32;
            return Some(ImageDimensions { width, height });
        }

        // Skip to next marker
        if pos + 4 > data.len() {
            return None;
        }
        let length = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        pos += 2 + length;
    }

    None
}

/// Get GIF dimensions from header.
pub fn get_gif_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    if data.len() < 10 {
        return None;
    }

    // GIF signature: GIF87a or GIF89a
    if &data[0..6] != b"GIF87a" && &data[0..6] != b"GIF89a" {
        return None;
    }

    // Width at offset 6 (2 bytes, little endian)
    let width = u16::from_le_bytes([data[6], data[7]]) as u32;
    // Height at offset 8 (2 bytes, little endian)
    let height = u16::from_le_bytes([data[8], data[9]]) as u32;

    Some(ImageDimensions { width, height })
}

/// Get WebP dimensions from header.
pub fn get_webp_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    if data.len() < 30 {
        return None;
    }

    // RIFF....WEBP
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return None;
    }

    // Check for VP8 chunk
    if &data[12..16] == b"VP8 " {
        // VP8 format
        let width = u16::from_le_bytes([data[26], data[27]]) as u32 & 0x3FFF;
        let height = u16::from_le_bytes([data[28], data[29]]) as u32 & 0x3FFF;
        return Some(ImageDimensions { width, height });
    }

    // Check for VP8L (lossless)
    if &data[12..16] == b"VP8L" {
        // VP8L format - dimensions are in a 28-bit field
        if data.len() < 21 {
            return None;
        }
        let bits = u32::from_le_bytes([data[17], data[18], data[19], data[20]]);
        let width = (bits & 0x3FFF) + 1;
        let height = ((bits >> 14) & 0x3FFF) + 1;
        return Some(ImageDimensions { width, height });
    }

    None
}

/// Get image dimensions for common formats.
pub fn get_image_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    // Try PNG first
    if let Some(dims) = get_png_dimensions(data) {
        return Some(dims);
    }

    // Try JPEG
    if let Some(dims) = get_jpeg_dimensions(data) {
        return Some(dims);
    }

    // Try GIF
    if let Some(dims) = get_gif_dimensions(data) {
        return Some(dims);
    }

    // Try WebP
    if let Some(dims) = get_webp_dimensions(data) {
        return Some(dims);
    }

    None
}

/// Calculate number of rows needed for an image.
pub fn calculate_image_rows(height: u32, cell_height: u16) -> u16 {
    height.div_ceil(cell_height as u32) as u16
}

/// Render image with fallback.
pub fn render_image(data: &[u8], options: &ImageRenderOptions) -> String {
    let caps = get_capabilities();

    match caps.protocol {
        ImageProtocol::Kitty => encode_kitty(data, options),
        ImageProtocol::ITerm2 => encode_iterm2(data, options),
        ImageProtocol::None => {
            // Fallback: show placeholder
            let alt_text = format!(
                "[Image {}x{}]",
                options.width.unwrap_or(1),
                options.height.unwrap_or(1)
            );
            alt_text
        }
    }
}

/// Image fallback for unsupported terminals.
pub fn image_fallback(_alt_text: &str, width: usize) -> String {
    let placeholder = "█".repeat(width.saturating_sub(2));
    format!(
        "╭{}╮\n│{}│\n╰{}╯",
        "─".repeat(width.saturating_sub(2)),
        placeholder,
        "─".repeat(width.saturating_sub(2))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_pi_image_terminals_and_disables_multiplexers() {
        for program in [
            "kitty",
            "ghostty",
            "wezterm",
            "WarpTerminal",
            "iTerm.app",
            "vscode",
        ] {
            let expected = match program {
                "iTerm.app" => ImageProtocol::ITerm2,
                "vscode" => ImageProtocol::None,
                _ => ImageProtocol::Kitty,
            };
            assert_eq!(
                detect_image_protocol(|key| (key == "TERM_PROGRAM").then(|| program.into())),
                expected
            );
            assert_eq!(
                detect_image_protocol(|key| match key {
                    "TERM_PROGRAM" => Some(program.into()),
                    "TMUX" => Some("session".into()),
                    _ => None,
                }),
                ImageProtocol::None
            );
        }
        assert_eq!(
            detect_image_protocol(|key| match key {
                "TERM" => Some("screen-256color".into()),
                "KITTY_WINDOW_ID" => Some("1".into()),
                _ => None,
            }),
            ImageProtocol::None
        );
    }

    #[test]
    fn kitty_graphics_have_zero_width_and_removed_placements_are_deleted() {
        let sequence = encode_kitty(
            b"png",
            &ImageRenderOptions {
                id: Some(42),
                width: Some(10),
                height: Some(5),
                ..Default::default()
            },
        );
        assert!(sequence.contains("C=1"));
        assert!(sequence.contains("c=10,r=5"));
        assert_eq!(crate::ansi::visible_width(&sequence), 0);
        assert_eq!(crate::utils::visible_width(&sequence), 0);
        assert_eq!(crate::utils::truncate_to_width(&sequence, 10, ""), sequence);
        assert!(removed_kitty_placements(&[sequence.clone()], &[sequence.clone()]).is_empty());
        assert_eq!(
            removed_kitty_placements(&[sequence], &["[Image]".into()]),
            delete_kitty_placement(42)
        );
    }

    #[test]
    fn test_capabilities_default() {
        let caps = TerminalCapabilities::default();
        assert_eq!(caps.protocol, ImageProtocol::None);
    }

    #[test]
    fn cell_dimensions_come_from_the_reported_pixel_size_or_stay_default() {
        use crossterm::terminal::WindowSize;

        let window = |columns, rows, width, height| WindowSize {
            rows,
            columns,
            width,
            height,
        };

        // The normal case: a 120x30 grid in a 1200x600 window is 10x20 per cell.
        assert_eq!(
            query_cell_dimensions(Some(window(120, 30, 1200, 600)), Some((120, 30))),
            Some(CellDimensions {
                width: 10,
                height: 20
            })
        );

        // Integer division truncates rather than rounding up, so a cell can
        // never be reported larger than the terminal actually has.
        assert_eq!(
            query_cell_dimensions(Some(window(120, 30, 1205, 605)), Some((120, 30))),
            Some(CellDimensions {
                width: 10,
                height: 20
            })
        );

        // Windows: `window_size()` is `Unsupported`, so no pixel size exists and
        // the historical default has to survive untouched. This is the case the
        // probe exists for — the previous code never left 8x16 anywhere.
        assert_eq!(query_cell_dimensions(None, Some((120, 30))), None);

        // A terminal that answers the query with zero pixels (common on Unix per
        // the ioctl docs) is not a usable measurement either.
        assert_eq!(
            query_cell_dimensions(Some(window(120, 30, 0, 0)), Some((120, 30))),
            None
        );

        // Fewer pixels than cells would imply a sub-pixel cell: reject it
        // instead of dividing down to a 0-width cell.
        assert_eq!(
            query_cell_dimensions(Some(window(120, 30, 100, 20)), Some((120, 30))),
            None
        );

        // A zero column count cannot be divided by; fall back to the window's
        // own grid, and reject when that is zero too.
        assert_eq!(
            query_cell_dimensions(Some(window(0, 0, 1200, 600)), Some((120, 30))),
            Some(CellDimensions {
                width: 10,
                height: 20
            })
        );
        assert_eq!(
            query_cell_dimensions(Some(window(0, 0, 1200, 600)), None),
            None
        );

        // The default itself is unchanged, so terminals without a pixel size
        // render exactly as they did before this change.
        assert_eq!(
            (
                TerminalCapabilities::default().cell_width,
                TerminalCapabilities::default().cell_height
            ),
            (DEFAULT_CELL_WIDTH, DEFAULT_CELL_HEIGHT)
        );
    }

    #[test]
    fn test_allocate_image_id() {
        let id1 = allocate_image_id();
        let id2 = allocate_image_id();
        assert!(id2 > id1);
    }

    #[test]
    fn test_hyperlink() {
        // let link = hyperlink("https://example.com", "Click here");
        // assert!(link.contains("https://example.com"));
        // assert!(link.contains("Click here"));
    }

    #[test]
    fn kitty_chunks_carry_the_more_data_flag() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        // A payload the size of a real screenshot: base64 is far past the
        // 4096-byte escape limit, so the encoder must split it.
        let mut payload = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        payload.resize(44_181, 0);
        let sequence = encode_kitty(
            &payload,
            &ImageRenderOptions {
                id: Some(11),
                width: Some(40),
                height: Some(8),
                ..Default::default()
            },
        );

        // A payload that fits in one escape stays a single, self-terminated
        // transfer: no continuation flag, but an explicit close.
        let single = encode_kitty(b"png", &ImageRenderOptions::default());
        assert_eq!(
            single.split("\x1b_G").count() - 1,
            1,
            "a small payload must not be split"
        );
        assert!(
            single.split(';').next().unwrap_or("").ends_with(",m=0"),
            "the single chunk must close the transfer with m=0, got {single:?}"
        );
        assert!(
            !single.contains("m=1"),
            "the single chunk must not claim more data follows"
        );

        let chunks: Vec<&str> = sequence.split("\x1b_G").skip(1).collect();
        assert!(
            chunks.len() > 1,
            "expected a multi-chunk transfer, got {} chunk(s)",
            chunks.len()
        );

        let mut reassembled = String::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let last = index == chunks.len() - 1;
            let body = chunk
                .strip_suffix("\x1b\\")
                .unwrap_or_else(|| panic!("chunk {index} is not terminated by ESC \\"));
            let (control, data) = body
                .split_once(';')
                .unwrap_or_else(|| panic!("chunk {index} has no data separator"));
            assert!(
                body.len() + 2 <= 4096,
                "chunk {index} is {} bytes; kitty allows at most 4096",
                body.len() + 2
            );
            // Every chunk but the last tells the terminal more data follows;
            // the last one closes the transfer.
            if last {
                assert!(
                    control.split(',').any(|field| field == "m=0"),
                    "last chunk must carry m=0, control was {control:?}"
                );
            } else {
                assert!(
                    control.split(',').any(|field| field == "m=1"),
                    "chunk {index} must carry m=1, control was {control:?}"
                );
            }
            // Only the first chunk describes the image; continuations are
            // limited to the more-data flag.
            if index == 0 {
                for key in ["a=T", "f=100", "i=11", "c=40", "r=8", "C=1"] {
                    assert!(
                        control.split(',').any(|field| field == key),
                        "first chunk is missing {key}, control was {control:?}"
                    );
                }
            } else {
                let expected = if last { "m=0" } else { "m=1" };
                assert_eq!(
                    control, expected,
                    "continuation chunk {index} must only carry the more-data flag"
                );
            }
            reassembled.push_str(data);
        }

        assert_eq!(
            reassembled,
            STANDARD.encode(&payload),
            "chunk payloads must concatenate back to the original base64"
        );
    }

    #[test]
    fn kitty_placements_are_cropped_to_the_rows_on_screen() {
        // 30 rows over 600 source pixels: 20 px per row.
        register_kitty_image(KittyImageMetadata {
            id: 9_001,
            columns: 10,
            rows: 30,
            width_px: 200,
            height_px: 600,
        });
        let line = encode_kitty(
            b"png",
            &ImageRenderOptions {
                id: Some(9_001),
                width: Some(10),
                height: Some(30),
                ..Default::default()
            },
        );

        assert!(is_image_line(&line));
        let metadata = kitty_image_metadata(&line).expect("registered placement");
        assert_eq!((metadata.columns, metadata.rows), (10, 30));

        // Fully visible: the placement is left alone, no source crop.
        assert_eq!(crop_kitty_image_line(&line, 0, 30), line);

        // Bottom clipped by the viewport: the source rectangle keeps the top
        // 8 rows (8 * 20 = 160 px) and the placement only covers 8 rows.
        let bottom_clipped = crop_kitty_image_line(&line, 0, 8);
        assert!(
            bottom_clipped.contains(",y=0,h=160,r=8;"),
            "expected a bottom crop, got {bottom_clipped:?}"
        );
        assert!(
            !bottom_clipped.contains(",r=30,"),
            "the rendered height must be replaced, got {bottom_clipped:?}"
        );

        // Top clipped by scrolling: rows 10..22 of the source are shown.
        let top_clipped = crop_kitty_image_line(&line, 10, 12);
        assert!(
            top_clipped.contains(",y=200,h=240,r=12;"),
            "expected a top crop, got {top_clipped:?}"
        );

        // A crop of a crop replaces the previous rectangle instead of stacking
        // two `y=`/`h=`/`r=` fields on the control string. Offsets stay relative
        // to the original image, not to the previous crop.
        let recropped = crop_kitty_image_line(&top_clipped, 2, 4);
        assert_eq!(
            recropped.matches(",y=").count(),
            1,
            "expected exactly one source offset, got {recropped:?}"
        );
        assert!(
            recropped.contains(",y=40,h=80,r=4;"),
            "the second crop must start from the original source rows, got {recropped:?}"
        );

        // Unregistered lines (plain text, iTerm2 payloads) are untouched.
        assert_eq!(crop_kitty_image_line("hello", 1, 2), "hello");
        assert!(!is_image_line("hello"));
        assert!(is_image_line("\x1b]1337;File=name=id1:AAAA\x07"));
    }

    #[test]
    fn a_repeated_image_row_is_placed_without_resending_the_payload() {
        register_kitty_image(KittyImageMetadata {
            id: 9_002,
            columns: 40,
            rows: 8,
            width_px: 400,
            height_px: 160,
        });
        let mut payload = vec![0x89, b'P', b'N', b'G'];
        payload.resize(44_181, 0);
        let transmitted = format!(
            "  {}",
            encode_kitty(
                &payload,
                &ImageRenderOptions {
                    id: Some(9_002),
                    width: Some(40),
                    height: Some(8),
                    ..Default::default()
                },
            )
        );
        let mut uploads = KittyUploads::new();

        // Nothing sent yet: the row has to carry its payload.
        assert_eq!(kitty_placement_line(&transmitted, &uploads), None);
        assert_eq!(kitty_transmission_id(&transmitted), Some(9_002));

        // The frame that transmits the payload is the frame that paints it; the
        // next frame may move the placement by id alone.
        uploads.note_painted(9_002);
        uploads.end_frame();
        let placed = kitty_placement_line(&transmitted, &uploads).expect("placement-only row");
        assert!(
            placed.starts_with("  \x1b_Ga=p,q=2,"),
            "the row must be rewritten in place, got {:?}",
            &placed[..placed.len().min(80)]
        );
        assert!(
            placed.contains(",i=9002,p=1,c=40,r=8"),
            "the placement keeps the image geometry, got {placed:?}"
        );
        assert!(
            !placed.contains("m=1") && !placed.contains("iVBOR"),
            "the payload must not be re-sent, got {} bytes",
            placed.len()
        );
        assert!(
            placed.len() < 200,
            "a placement row is tiny compared to the payload, got {} bytes",
            placed.len()
        );

        // A cropped row keeps its source rectangle.
        let cropped = crop_kitty_image_line(&transmitted, 2, 4);
        let placed = kitty_placement_line(&cropped, &uploads).expect("placement-only crop");
        assert!(
            placed.contains(",y=40,h=80,r=4"),
            "the crop must survive the rewrite, got {placed:?}"
        );

        // After a screen clear the payload has to be sent again.
        uploads.invalidate();
        assert_eq!(kitty_placement_line(&transmitted, &uploads), None);
    }

    /// The sequence a scroll step produces: the old row is repainted, the new
    /// row carries the same image. Deleting the placement here would also drop
    /// the payload the placement-only rewrite depends on, so the move must not
    /// delete anything — the `(i=, p=)` pair replaces the placement in place.
    #[test]
    fn moving_an_image_does_not_delete_the_payload_it_re_places() {
        register_kitty_image(KittyImageMetadata {
            id: 9_003,
            columns: 40,
            rows: 8,
            width_px: 400,
            height_px: 800,
        });
        let mut payload = vec![0x89, b'P', b'N', b'G'];
        payload.resize(44_181, 0);
        let transmitted = format!(
            "  {}",
            encode_kitty(
                &payload,
                &ImageRenderOptions {
                    id: Some(9_003),
                    width: Some(40),
                    height: Some(8),
                    ..Default::default()
                },
            )
        );
        let mut uploads = KittyUploads::new();
        uploads.note_painted(9_003);
        uploads.end_frame();

        // Frame N: the image is anchored on row 4 of the viewport.
        let mut previous: Vec<String> = (0..6).map(|row| format!("row {row}")).collect();
        previous[4] = transmitted.clone();
        // Frame N+1: it scrolled up one row.
        let mut current: Vec<String> = (0..6).map(|row| format!("row {row}")).collect();
        current[3] = transmitted.clone();

        let deletes = removed_kitty_placements(&previous, &current);
        assert!(
            deletes.is_empty(),
            "an image that is still on screen is moved, not deleted: {deletes:?}"
        );

        let moved = kitty_placement_line(&current[3], &uploads).expect("placement-only move");
        assert!(moved.contains("\x1b_Ga=p,"), "got {moved:?}");
        assert!(
            moved.contains("i=9003,p=1"),
            "the move must reuse the placement id, got {moved:?}"
        );
        assert!(
            !moved.contains("iVBOR"),
            "the move must not re-send the payload, got {} bytes",
            moved.len()
        );
    }

    /// An image that leaves the screen is deleted by placement only: the stored
    /// data has to survive for a later re-place of the same id.
    #[test]
    fn a_placement_that_leaves_the_screen_is_deleted_without_freeing_its_data() {
        let transmitted = encode_kitty(
            b"png",
            &ImageRenderOptions {
                id: Some(9_004),
                width: Some(20),
                height: Some(4),
                ..Default::default()
            },
        );
        let previous = vec!["row 0".to_string(), transmitted, "row 2".to_string()];
        let current = vec![
            "row 0".to_string(),
            "row 1".to_string(),
            "row 2".to_string(),
        ];

        let deletes = removed_kitty_placements(&previous, &current);
        assert_eq!(deletes, delete_kitty_placement(9_004));
        assert!(
            !deletes.contains("d=I"),
            "the uppercase form frees the data and would blank a later re-place: {deletes:?}"
        );
        assert!(
            deletes.contains("i=9004,p=1"),
            "only this client's placement may be deleted: {deletes:?}"
        );
    }

    #[test]
    fn upload_state_follows_the_frame_and_the_terminal() {
        let mut uploads = KittyUploads::new();
        assert!(!uploads.is_uploaded(7));
        uploads.note_painted(7);
        assert!(
            !uploads.is_uploaded(7),
            "the frame that transmits the payload must still write it"
        );
        uploads.end_frame();
        assert!(uploads.is_uploaded(7));

        // A frame that does not paint the image forgets it: when it comes back
        // the payload is re-sent instead of trusting a store nobody watched.
        uploads.end_frame();
        assert!(!uploads.is_uploaded(7));
        uploads.note_painted(7);
        uploads.end_frame();
        assert!(uploads.is_uploaded(7));

        uploads.invalidate();
        assert!(!uploads.is_uploaded(7), "a screen clear drops every image");
    }

    /// Placement geometry must stay resolvable for as long as a document can
    /// retain the row: scrolling back to an old image has to keep its crop.
    #[test]
    fn placement_metadata_outlives_a_long_transcript() {
        for id in 20_000..20_600 {
            register_kitty_image(KittyImageMetadata {
                id,
                columns: 10,
                rows: 8,
                width_px: 100,
                height_px: 80,
            });
        }
        let line = encode_kitty(
            b"png",
            &ImageRenderOptions {
                id: Some(20_000),
                width: Some(10),
                height: Some(8),
                ..Default::default()
            },
        );
        assert!(
            kitty_image_metadata(&line).is_some(),
            "the oldest placement must stay resolvable"
        );
        let cropped = crop_kitty_image_line(&line, 2, 4);
        assert!(
            cropped.contains(",y=20,h=40,r=4;"),
            "expected the crop of the oldest image, got {cropped:?}"
        );
    }

    #[test]
    fn test_png_dimensions() {
        // Minimal PNG header
        let data = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // IHDR
            0x00, 0x00, 0x00, 0x10, // width = 16
            0x00, 0x00, 0x00, 0x20, // height = 32
            0x08, 0x02, 0x00, 0x00, 0x00, // bit depth, color type, etc.
        ];
        let dims = get_png_dimensions(&data).unwrap();
        assert_eq!(dims.width, 16);
        assert_eq!(dims.height, 32);
    }
}
