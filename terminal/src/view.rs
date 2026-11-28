//! The GPUI element that owns a terminal session.
//!
//! Feature set, modelled on Zed's `terminal` / `terminal_view` crates:
//!
//! * Full alacritty VTE emulation with a real, configurable scrollback.
//! * Terminal queries answered on the PTY (cursor position, device attributes,
//!   text-area size, colour queries) so TUIs behave.
//! * Auto-scroll: output pins the viewport to the bottom, typing jumps back to
//!   the bottom, dragging a selection past the edge keeps scrolling.
//! * Alacritty-backed selection: character, word (semantic), line and block,
//!   scrollback aware, with optional copy-on-select.
//! * Mouse reporting (X10/SGR/UTF-8), motion & drag reporting, alternate
//!   scroll, shift to bypass.
//! * Hyperlinks: `OSC 8`, URLs and `path:line:col` targets, underlined on
//!   hover, opened on modifier-click.
//! * Buffer search with match highlighting and next/previous.
//! * Vi mode: motions, visual selection, yank.
//! * Blinking cursor with all cursor shapes, focus reporting (mode 1004).
//! * Bracketed paste, `OSC 52` clipboard read/write, bell and title events.

use crate::AgentStatusReport;
use crate::colors::ColorPalette;
use crate::event::{GpuiEventProxy, TerminalEvent, TerminalNotifier};
use crate::hyperlink::{HyperlinkMatch, RegexSearches};
use crate::input::keystroke_to_bytes;
use crate::pty_writer::{InputStarter, QueuedWriter, WriteCompletion};
use crate::render::{PaintContext, TerminalRenderer};
use crate::search::{SearchSnapshot, SearchState};
use crate::shell_integration::ShellEvent;
use crate::terminal::TerminalState;
use alacritty_terminal::event::WindowSize as PtyWindowSize;
use alacritty_terminal::grid::{Dimensions as GridDimensions, Scroll as GridScroll};
use alacritty_terminal::index::{
    Column as GridColumn, Direction as GridDirection, Line as GridLine, Point as GridPoint,
    Side as GridSide,
};
use alacritty_terminal::selection::{
    Selection as GridSelection, SelectionRange, SelectionType as GridSelectionType,
};
use alacritty_terminal::term::search::Match as GridMatch;
use alacritty_terminal::term::{TermMode, viewport_to_point};
use alacritty_terminal::vi_mode::ViMotion;
use gpui::prelude::FluentBuilder as _;
use gpui::{Edges, *};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// How long a drag-past-the-edge waits between auto-scroll steps.
const DRAG_SCROLL_INTERVAL: Duration = Duration::from_millis(40);

/// Width of the invisible hit area on the right edge that grabs the scrollbar.
const SCROLLBAR_HIT_WIDTH: Pixels = px(14.0);

#[derive(Clone, Debug)]
pub struct TerminalConfig {
    pub cols: usize,
    pub rows: usize,
    pub font_family: String,
    pub font_size: Pixels,
    /// Number of lines of scrollback to retain. This is now actually honoured —
    /// it is passed to alacritty's `Config::scrolling_history`.
    pub scrollback: usize,
    pub line_height_multiplier: f32,
    pub padding: Edges<Pixels>,
    pub colors: ColorPalette,

    /// Per-run token required on OSC 133 markers. Empty accepts all markers
    /// (standalone use and tests).
    pub shell_token: String,

    /// Blink the cursor when the application asks for a blinking cursor.
    pub cursor_blink: bool,
    /// Half-period of the cursor blink animation.
    pub blink_interval: Duration,
    /// Copy the selection to the clipboard as soon as the mouse is released.
    pub copy_on_select: bool,
    /// Preserve the component's legacy right-click copy/paste gesture.
    /// Embedders with a context menu should disable this so the same click is
    /// not also forwarded to the PTY or pasted before the menu opens.
    pub right_click_paste: bool,
    /// Translate wheel events into arrow keys while the alternate screen is
    /// active and the application is not doing its own mouse reporting.
    pub alternate_scroll: bool,
    /// Multiplier applied to wheel deltas.
    pub scroll_sensitivity: f32,
    /// Characters that terminate a semantic (double-click) selection.
    pub semantic_escape_chars: String,
    /// Paint the overlay scrollbar.
    pub show_scrollbar: bool,
    /// Maximum number of lines a single wheel event may scroll.
    pub max_scroll_lines: i32,
    /// Allow applications to *read* the clipboard through `OSC 52`.
    pub allow_osc52_paste: bool,
    /// Also treat `path:line:column` runs as clickable links, not just URLs.
    pub detect_path_links: bool,
    /// Overlay colour painted over selected cells.
    pub selection_color: Hsla,
    /// Overlay colour painted over search matches.
    pub search_match_color: Hsla,
    /// Overlay colour painted over the focused search match.
    pub active_search_match_color: Hsla,
    /// Disallow direct user typing on the terminal grid (read-only mode).
    pub read_only: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            font_family: "monospace".into(),
            font_size: px(14.0),
            scrollback: 10_000,
            line_height_multiplier: 1.2,
            padding: Edges::all(px(0.0)),
            colors: ColorPalette::default(),
            shell_token: String::new(),
            cursor_blink: true,
            blink_interval: Duration::from_millis(500),
            copy_on_select: false,
            right_click_paste: true,
            alternate_scroll: true,
            scroll_sensitivity: 1.0,
            semantic_escape_chars: ",│`|:\"' ()[]{}<>\t".into(),
            show_scrollbar: true,
            max_scroll_lines: 10,
            allow_osc52_paste: false,
            detect_path_links: true,
            selection_color: hsla(215.0 / 360.0, 0.85, 0.45, 0.35),
            search_match_color: hsla(45.0 / 360.0, 0.9, 0.5, 0.30),
            active_search_match_color: hsla(25.0 / 360.0, 0.95, 0.55, 0.55),
            read_only: false,
        }
    }
}

pub type ResizeCallback = Box<dyn Fn(usize, usize) + Send + Sync>;
pub type KeyHandler = Box<dyn Fn(&KeyDownEvent) -> bool + Send + Sync>;
pub type BellCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;
pub type TitleCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;
pub type ClipboardStoreCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;
pub type ExitCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;
/// Invoked when a hyperlink is activated. Return `true` to mark it handled and
/// suppress the built-in `open_url` fallback.
pub type LinkCallback =
    Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &HyperlinkMatch) -> bool>;

const MAX_DEFERRED_WINDOW_EVENTS: usize = 64;
const MAX_DEFERRED_WINDOW_TEXT_BYTES: usize = 256 * 1024;

#[derive(Default)]
struct DeferredWindowEvents {
    events: VecDeque<TerminalEvent>,
    text_bytes: usize,
}

impl DeferredWindowEvents {
    fn text_bytes(event: &TerminalEvent) -> usize {
        match event {
            TerminalEvent::Title(text) | TerminalEvent::ClipboardStore(_, text) => text.capacity(),
            _ => 0,
        }
    }

    fn push(&mut self, event: TerminalEvent) {
        let bytes = Self::text_bytes(&event);
        if bytes > MAX_DEFERRED_WINDOW_TEXT_BYTES {
            return;
        }
        while self.events.len() == MAX_DEFERRED_WINDOW_EVENTS
            || self.text_bytes > MAX_DEFERRED_WINDOW_TEXT_BYTES - bytes
        {
            // There is at most one Exit, and the background drain has already
            // observed it. Evict the oldest other event, never that marker.
            let index = if matches!(self.events.front(), Some(TerminalEvent::Exit)) {
                1
            } else {
                0
            };
            let Some(evicted) = self.events.remove(index) else {
                return;
            };
            self.text_bytes -= Self::text_bytes(&evicted);
        }
        self.text_bytes += bytes;
        self.events.push_back(event);
    }

    fn pop(&mut self) -> Option<TerminalEvent> {
        let event = self.events.pop_front()?;
        self.text_bytes -= Self::text_bytes(&event);
        Some(event)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct HoverRevision {
    screen: u64,
    display_offset: usize,
}

#[derive(PartialEq, Eq)]
struct HoveredLink {
    revision: HoverRevision,
    link: HyperlinkMatch,
}

fn update_selection_endpoint(
    selection: &mut GridSelection,
    point: GridPoint,
    side: GridSide,
) -> bool {
    // Alacritty's endpoints are private; compare the small, allocation-free
    // value rather than materializing a range (which can scan a wrapped line).
    let previous = selection.clone();
    selection.update(point, side);
    *selection != previous
}

fn scroll_lines_if_changed(state: &TerminalState, lines: i32) -> bool {
    if lines == 0 {
        return false;
    }
    state.with_term_mut(|term| {
        let offset = term.grid().display_offset();
        if (lines < 0 && offset == 0) || (lines > 0 && offset == term.grid().history_size()) {
            return false;
        }
        term.scroll_display(GridScroll::Delta(lines));
        term.grid().display_offset() != offset
    })
}

pub struct TerminalView {
    state: TerminalState,
    renderer: std::rc::Rc<TerminalRenderer>,
    focus_handle: FocusHandle,
    stdin_writer: QueuedWriter,
    /// Independent of stdout: a failed input write must wake a silent terminal.
    _input_error_task: Task<()>,
    io_error: Option<String>,
    event_rx: mpsc::Receiver<TerminalEvent>,
    config: TerminalConfig,

    #[allow(dead_code)]
    _reader_task: Task<()>,
    start_reader: Option<Box<dyn FnOnce() + Send>>,
    start_writer: InputStarter,
    start_callback: Option<Box<dyn Fn() + Send + Sync>>,
    started: bool,
    blink_task: Option<Task<()>>,
    blink_restart_pending: bool,
    drag_scroll_task: Option<Task<()>>,

    /// Shared with the event proxy so `CSI 14 t` / `CSI 18 t` can be answered.
    window_size: Arc<parking_lot::Mutex<PtyWindowSize>>,
    /// Shared with the event proxy so `OSC 4/10/11/12` can be answered.
    palette_handle: Arc<parking_lot::Mutex<ColorPalette>>,
    /// Bounds of the last paint, in window coordinates.
    last_bounds: Arc<parking_lot::Mutex<Bounds<Pixels>>>,
    /// Grid size seen by the last render. The paint pass resizes the grid, so
    /// comparing this against the live size is how a render notices that the
    /// grid moved under it (search matches are stored in grid coordinates).
    last_grid_size: (usize, usize),
    /// Increments whenever PTY output or a grid resize can change the visible
    /// screen. The status poller uses this to skip scraping unchanged panes.
    screen_revision: Arc<AtomicU64>,
    /// Diagnostics only: oldest output not yet painted by this view.
    pending_output: Arc<parking_lot::Mutex<Option<crate::diagnostics::PendingOutput>>>,

    resize_callback: Option<Arc<ResizeCallback>>,
    key_handler: Option<Arc<KeyHandler>>,
    bell_callback: Option<BellCallback>,
    title_callback: Option<TitleCallback>,
    clipboard_store_callback: Option<ClipboardStoreCallback>,
    exit_callback: Option<ExitCallback>,
    link_callback: Option<LinkCallback>,

    scroll_accumulator: f32,
    scrollbar_dragging: bool,
    scrollbar_hovered: bool,
    mouse_down_button: Option<MouseButton>,
    last_reported_cell: Option<GridPoint>,
    last_mouse_position: Point<Pixels>,
    cell_metrics_valid: bool,

    search: SearchState,
    regex_searches: Option<RegexSearches>,
    hovered_link: Option<HoveredLink>,

    cursor_visible: bool,
    was_focused: bool,
    title: Option<String>,
    exit_status: Option<i32>,
    exited: bool,
    read_only: bool,
    /// Events a background drain (`drain_background_events`) could not fully
    /// process and is saving for the next foreground render.
    deferred_events: DeferredWindowEvents,
    /// Last time search matches were recomputed; rescans are throttled while
    /// output is streaming (see `SEARCH_REFRESH_MIN_INTERVAL`).
    search_last_refresh: Option<std::time::Instant>,
    /// A throttled rescan is owed once the throttle interval has elapsed.
    search_refresh_pending: bool,
    /// Wakes the owed trailing rescan even when no further output/frame arrives.
    search_refresh_task: Option<Task<()>>,
    search_epoch: u64,
    search_select_first: bool,
    /// Last time this view rendered. Used to tell a visible terminal from a
    /// background one, which does not need live search matches.
    last_render: Option<std::time::Instant>,

    /// Hibernated panes drop their grid and history to a stub: the text below,
    /// plus the PTY still attached (never killed) and its output buffered in a
    /// bounded backlog. Shared with the reader task so it can skip parsing and
    /// skip waking the UI entirely.
    hibernated: Arc<std::sync::atomic::AtomicBool>,
    /// The pane's history as text, kept in memory so waking needs no disk read.
    /// Bounded by [`MAX_HIBERNATED_TEXT_BYTES`].
    hibernated_text: String,
    /// Raw PTY bytes that arrived while hibernated. Parsing resumes with these
    /// on wake; the bound is what keeps a noisy background process harmless.
    hibernated_backlog: Arc<parking_lot::Mutex<Vec<u8>>>,
}

/// History kept for a hibernated pane. Matches the app's persisted scrollback
/// cap, so waking shows what a restart would have shown anyway.
const MAX_HIBERNATED_TEXT_LINES: usize = 2_000;
/// The text is the only per-pane memory a hibernated pane keeps; cap it so a
/// pane that printed a giant line cannot defeat hibernation.
const MAX_HIBERNATED_TEXT_BYTES: usize = 512 * 1024;
/// PTY output buffered while hibernated before the oldest whole lines are
/// discarded. A quiet shell (the only kind eligible to hibernate) sends nothing.
const MAX_HIBERNATED_BACKLOG_BYTES: usize = 256 * 1024;
/// A hibernated grid retains no scrollback of its own; anything it accumulates
/// is dropped on the next wake replay and must not grow unnoticed.
const HIBERNATED_GRID_HISTORY: usize = 0;

/// Estimated cost of a terminal view, for the app's memory overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalViewMemory {
    pub grid: crate::terminal::TerminalMemory,
    pub hibernated_text_bytes: usize,
    pub backlog_bytes: usize,
    pub renderer_bytes: usize,
}

/// Minimum gap between full-scrollback search rescans while output streams in.
const SEARCH_REFRESH_MIN_INTERVAL: Duration = Duration::from_millis(500);
const SEARCH_SNAPSHOT_CELLS_PER_SLICE: usize = 8_192;

/// A view that has not rendered for this long is treated as hidden. Background
/// terminals still parse their output (the grid must stay current), but they do
/// not rescan the whole scrollback for search matches; the first render after
/// they reappear rebuilds the match list.
const SEARCH_VISIBLE_WINDOW: Duration = Duration::from_secs(1);

/// Upper bound on the bytes one UI-thread parse may cover. The reader already
/// coalesces whatever is queued so a firehose costs one parse and one repaint
/// instead of hundreds, but without a cap the batch (and the frame it stalls)
/// grows with the backlog. At the measured parse throughput a full 128 KiB
/// batch is ~4 ms, comfortably inside a 60 fps frame. The remainder stays
/// queued and is picked up by the next loop iteration.
const MAX_COALESCE_BYTES: usize = 128 * 1024;
const PTY_READ_BYTES: usize = 32 * 1024;
const PTY_RECYCLED_BUFFERS: usize = 8;

/// Give a fast producer a moment to queue more before parsing, so a trickle of
/// small reads becomes a few larger batches instead of tens of thousands of
/// tiny UI updates (measured: ~35k updates/s at 38 MB/s without it). Applied
/// only when the reader has caught up; a backlog is parsed immediately. One
/// millisecond is below interactive perceptibility.
const COALESCE_DELAY: Duration = Duration::from_millis(1);

async fn yield_to_ui() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if std::mem::replace(&mut yielded, true) {
            std::task::Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

struct OutputChunk {
    buffer: Vec<u8>,
    len: usize,
    read_at: Option<std::time::Instant>,
}

impl OutputChunk {
    fn bytes(&self) -> &[u8] {
        &self.buffer[..self.len]
    }
}

/// Append PTY bytes to the hibernation backlog, keeping at most
/// [`MAX_HIBERNATED_BACKLOG_BYTES`]. Trimming happens at a line boundary so a
/// replay does not start inside an escape sequence; a single unbroken line
/// larger than the budget falls back to its newest bytes.
fn append_bounded(backlog: &mut Vec<u8>, bytes: &[u8]) {
    backlog.extend_from_slice(bytes);
    let cap = MAX_HIBERNATED_BACKLOG_BYTES;
    if backlog.len() <= cap {
        return;
    }
    let overflow = backlog.len() - cap;
    if let Some(offset) = backlog[overflow..].iter().position(|&byte| byte == b'\n') {
        backlog.drain(..overflow + offset + 1);
    } else {
        let cut = backlog.len() - cap;
        backlog.drain(..cut);
    }
}

/// Keep the newest `cap` bytes, at a UTF-8 boundary.
fn trim_front_to_bytes(text: &mut String, cap: usize) {
    if text.len() <= cap {
        return;
    }
    let mut cut = text.len() - cap;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    text.drain(..cut);
}

/// Last `max_lines` complete lines of a text buffer.
fn tail_lines(text: &str, max_lines: usize) -> String {
    if max_lines == 0 {
        return String::new();
    }
    let total = text.bytes().filter(|&byte| byte == b'\n').count();
    if total <= max_lines {
        return text.to_string();
    }
    let skip = total - max_lines;
    let mut seen = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            seen += 1;
            if seen == skip {
                return text[index + 1..].to_string();
            }
        }
    }
    text.to_string()
}

impl TerminalView {
    pub fn new<W, R>(
        stdin_writer: W,
        stdout_reader: R,
        config: TerminalConfig,
        cx: &mut Context<Self>,
    ) -> Self
    where
        W: Write + Send + 'static,
        R: Read + Send + 'static,
    {
        Self::new_with_scrollback(stdin_writer, stdout_reader, config, None, cx)
    }

    /// Like [`Self::new`], but seeds the emulator with previously captured
    /// output in bounded, yielding batches before parsing live stdout, so a
    /// restored pane shows its old buffer above the fresh shell prompt.
    pub fn new_with_scrollback<W, R>(
        stdin_writer: W,
        stdout_reader: R,
        config: TerminalConfig,
        scrollback: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self
    where
        W: Write + Send + 'static,
        R: Read + Send + 'static,
    {
        let (event_tx, event_rx) = mpsc::channel();
        let exit_event_tx = event_tx.clone();

        let event_proxy = GpuiEventProxy::new(event_tx);
        let notifier_slot = event_proxy.notifier_slot();

        let alac_config = TerminalState::build_config(
            config.scrollback,
            &config.semantic_escape_chars,
            config.allow_osc52_paste,
        );
        let mut state =
            TerminalState::with_config(config.cols, config.rows, alac_config, event_proxy);
        state.set_palette(config.colors.clone());
        state.set_shell_token(config.shell_token.clone());

        let renderer = std::rc::Rc::new(TerminalRenderer::new(
            config.font_family.clone(),
            config.font_size,
            config.line_height_multiplier,
            config.colors.clone(),
        ));

        let focus_handle = cx.focus_handle();

        let (stdin_writer, input_errors, start_writer) =
            QueuedWriter::deferred(stdin_writer, cx.background_executor());
        let input_error_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Ok(error) = input_errors.recv_async().await {
                if this
                    .update(cx, |view, cx| view.report_io_error(error, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let window_size = Arc::new(parking_lot::Mutex::new(PtyWindowSize {
            num_lines: config.rows as u16,
            num_cols: config.cols as u16,
            cell_width: f32::from(renderer.cell_width).max(1.0) as u16,
            cell_height: f32::from(renderer.cell_height).max(1.0) as u16,
        }));
        let palette_handle = Arc::new(parking_lot::Mutex::new(config.colors.clone()));

        // Install only after restoration: old scrollback queries must not send
        // replies into the new shell. Live queries then share the input FIFO.
        let notifier = TerminalNotifier::queued(
            stdin_writer.clone(),
            Arc::clone(&window_size),
            Arc::clone(&palette_handle),
        );

        // Bounded so a firehose process (`cat` a huge file) applies backpressure
        // to the reader thread — and through it to the PTY — instead of
        // queueing an unbounded number of byte chunks in memory.
        let (bytes_tx, bytes_rx) = flume::bounded::<OutputChunk>(256);
        let (recycle_tx, recycle_rx) = flume::bounded(PTY_RECYCLED_BUFFERS);

        let start_reader: Box<dyn FnOnce() + Send> = Box::new(move || {
            thread::spawn(move || Self::read_stdout_blocking(stdout_reader, bytes_tx, recycle_rx));
        });

        let hibernated = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hibernated_backlog: Arc<parking_lot::Mutex<Vec<u8>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let reader_hibernated = Arc::clone(&hibernated);
        let reader_backlog = Arc::clone(&hibernated_backlog);

        let reader_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            if let Some(scrollback) = scrollback.filter(|text| !text.is_empty()) {
                for bytes in scrollback.as_bytes().chunks(MAX_COALESCE_BYTES) {
                    if this
                        .update(cx, |view, cx| {
                            // Raw restoration deliberately bypasses shell markers,
                            // agent OSC extraction and capture, like the old seed path.
                            view.state.feed_raw(bytes);
                            view.screen_revision.fetch_add(1, Ordering::Relaxed);
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                    yield_to_ui().await;
                }
                if this
                    .update(cx, |view, cx| {
                        view.state.scroll_to_bottom();
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            *notifier_slot.lock() = Some(notifier);
            let mut pending = None;
            let mut batch = Vec::new();
            loop {
                let next = match pending.take() {
                    Some(bytes) => Ok(bytes),
                    None => bytes_rx.recv_async().await,
                };
                match next {
                    Ok(chunk) => {
                        // A hibernated pane has no grid to parse into. Buffer
                        // the newest bounded window for the wake replay and do
                        // not touch the UI: no parse, no notify, no paint.
                        if reader_hibernated.load(Ordering::Acquire) {
                            batch = Vec::new();
                            append_bounded(&mut reader_backlog.lock(), chunk.bytes());
                            let _ = recycle_tx.try_send(chunk.buffer);
                            continue;
                        }
                        // Coalesce what already arrived so a firehose of output
                        // costs one parse + one repaint, not hundreds — but stop
                        // at a budget so the parse, which runs on the UI thread,
                        // cannot grow with the queued backlog. The rest stays in
                        // the channel for the next iteration.
                        //
                        // Wait only when the reader has caught up with the PTY.
                        // A non-empty queue means the producer is already ahead,
                        // so parsing immediately keeps bulk throughput intact;
                        // the pause is only there to batch the trickle a fast
                        // producer writes between wakeups.
                        if bytes_rx.is_empty() {
                            cx.background_executor().timer(COALESCE_DELAY).await;
                        }
                        batch.clear();
                        while batch.len().max(chunk.len) < MAX_COALESCE_BYTES {
                            let Ok(more) = bytes_rx.try_recv() else {
                                break;
                            };
                            if more.len > MAX_COALESCE_BYTES - batch.len().max(chunk.len) {
                                pending = Some(more);
                                break;
                            }
                            if batch.is_empty() {
                                batch.reserve(MAX_COALESCE_BYTES);
                                batch.extend_from_slice(chunk.bytes());
                            }
                            batch.extend_from_slice(more.bytes());
                            let _ = recycle_tx.try_send(more.buffer);
                        }
                        let bytes = if batch.is_empty() {
                            chunk.bytes()
                        } else {
                            batch.as_slice()
                        };
                        let read_at = chunk.read_at;

                        let result = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                            crate::diagnostics::output_queued(read_at, bytes_rx.len());
                            let diag_parse = crate::diagnostics::parse_start();
                            let shell_events = view.state.process_bytes(bytes);
                            crate::diagnostics::parse_end(diag_parse, bytes.len());
                            crate::diagnostics::output_parsed(&view.pending_output, read_at);
                            for event in shell_events {
                                cx.emit(event);
                            }
                            view.screen_revision.fetch_add(1, Ordering::Relaxed);
                            // Hidden terminals do not need a live match list;
                            // the first render after they reappear refreshes it.
                            if view.search.is_active() && view.visible_recently() {
                                // A full-scrollback rescan per batch would stall
                                // the UI thread under streaming output; throttle
                                // it and arm a timer for the owed trailing pass.
                                view.refresh_streaming_search(cx);
                            }
                            cx.notify();
                        });
                        let _ = recycle_tx.try_send(chunk.buffer);
                        if result.is_err() {
                            break;
                        }
                        // recv_async can remain ready forever under a firehose.
                        // Explicitly return to GPUI between bounded parse batches.
                        yield_to_ui().await;
                    }
                    Err(_) => {
                        let _ = exit_event_tx.send(TerminalEvent::Exit);
                        let _ = this.update(cx, |_view, cx: &mut Context<Self>| {
                            cx.notify();
                        });
                        break;
                    }
                }
            }
        });

        let read_only = config.read_only;
        Self {
            state,
            renderer,
            focus_handle,
            stdin_writer,
            _input_error_task: input_error_task,
            io_error: None,
            event_rx,
            config,
            _reader_task: reader_task,
            start_reader: Some(start_reader),
            start_writer,
            start_callback: None,
            started: false,
            blink_task: None,
            blink_restart_pending: false,
            drag_scroll_task: None,
            window_size,
            palette_handle,
            last_bounds: Arc::new(parking_lot::Mutex::new(Bounds::default())),
            last_grid_size: (0, 0),
            screen_revision: Arc::new(AtomicU64::new(0)),
            pending_output: Arc::new(parking_lot::Mutex::new(None)),
            resize_callback: None,
            key_handler: None,
            bell_callback: None,
            title_callback: None,
            clipboard_store_callback: None,
            exit_callback: None,
            link_callback: None,
            scroll_accumulator: 0.0,
            scrollbar_dragging: false,
            scrollbar_hovered: false,
            mouse_down_button: None,
            last_reported_cell: None,
            last_mouse_position: Point::default(),
            cell_metrics_valid: false,
            search: SearchState::new(),
            regex_searches: None,
            hovered_link: None,
            cursor_visible: true,
            was_focused: false,
            title: None,
            exit_status: None,
            exited: false,
            deferred_events: DeferredWindowEvents::default(),
            search_last_refresh: None,
            search_refresh_pending: false,
            search_refresh_task: None,
            search_epoch: 0,
            search_select_first: false,
            last_render: None,
            hibernated,
            hibernated_text: String::new(),
            hibernated_backlog,
            read_only,
        }
    }

    // -----------------------------------------------------------------------
    // Builder-style callbacks
    // -----------------------------------------------------------------------

    pub fn with_start_callback(mut self, callback: impl Fn() + Send + Sync + 'static) -> Self {
        self.start_callback = Some(Box::new(callback));
        self
    }

    /// Start live I/O only when rendered or explicitly sent input.
    pub fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        if let Some(callback) = &self.start_callback {
            callback();
        }
        self.start_writer.start();
        if let Some(start_reader) = self.start_reader.take() {
            start_reader();
        }
    }

    /// Discard only resources which a foreground render can reconstruct.
    pub fn release_idle_resources(&mut self) {
        self.blink_task = None;
        self.search_epoch = self.search_epoch.wrapping_add(1);
        self.search_select_first = false;
        self.drag_scroll_task = None;
        self.search_refresh_pending = false;
        self.search_last_refresh = None;
        self.last_render = None;
        self.mouse_down_button = None;
        self.last_reported_cell = None;
        self.scrollbar_dragging = false;
        self.scrollbar_hovered = false;
        self.hovered_link = None;
        self.renderer.release_idle_resources();
        self.regex_searches = None;
        self.search.release_idle_resources();
    }

    /// Hibernate: replace the grid with an empty one and keep only bounded
    /// history text, leaving the PTY (and whatever it is running) untouched.
    /// Returns the text the caller should persist for crash safety, or `None`
    /// when the pane is already hibernated.
    ///
    /// Ownership of the process never changes here: the shell keeps running,
    /// its output is buffered in a bounded backlog, and waking replays that
    /// backlog into a rebuilt grid.
    pub fn hibernate(&mut self) -> Option<String> {
        if self.hibernated.load(Ordering::Acquire) {
            return None;
        }
        let mut text = self.state.buffer_text(MAX_HIBERNATED_TEXT_LINES);
        trim_front_to_bytes(&mut text, MAX_HIBERNATED_TEXT_BYTES);
        // Set the flag before touching the grid so the reader starts buffering
        // instead of parsing into a grid that is about to disappear.
        self.hibernated.store(true, Ordering::Release);
        self.state.reset_grid(HIBERNATED_GRID_HISTORY);
        self.release_idle_resources();
        // Whatever had arrived is already inside `text`, so the post-hibernation
        // backlog starts empty. Deferred events are kept: an exit or a title
        // that arrived just before sleeping must still apply on wake.
        self.hibernated_backlog.lock().clear();
        self.hibernated_text = text.clone();
        Some(text)
    }

    pub fn is_hibernated(&self) -> bool {
        self.hibernated.load(Ordering::Acquire)
    }

    /// `true` once the PTY reader and input worker have been started.
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// OS threads owned by this pane: the PTY reader and the input worker.
    /// Transient PTY setup work runs on the shared pool and is not counted.
    pub fn thread_count(&self) -> usize {
        if self.started { 2 } else { 0 }
    }

    /// Wake: restore the scrollback setting and replay the kept text plus any
    /// output that arrived while asleep. Cheap enough for a render pass.
    pub fn wake(&mut self, cx: &mut Context<Self>) {
        if self.wake_inner() {
            cx.notify();
        }
    }

    fn wake_inner(&mut self) -> bool {
        if !self.hibernated.load(Ordering::Acquire) {
            return false;
        }
        self.hibernated.store(false, Ordering::Release);
        self.state.set_grid_history(self.config.scrollback);
        let history = std::mem::take(&mut self.hibernated_text);
        let backlog = std::mem::take(&mut *self.hibernated_backlog.lock());
        if !history.is_empty() {
            self.state.feed_raw(history.as_bytes());
        }
        if !backlog.is_empty() {
            self.state.feed_raw(&backlog);
        }
        self.state.scroll_to_bottom();
        self.screen_revision.fetch_add(1, Ordering::Relaxed);
        self.reset_search();
        self.regex_searches = None;
        true
    }

    /// Estimated per-view memory. `grid` dominates until the pane hibernates;
    /// after that only the bounded text and backlog remain.
    pub fn memory_usage(&self) -> TerminalViewMemory {
        TerminalViewMemory {
            grid: self.state.memory_usage(),
            hibernated_text_bytes: self.hibernated_text.len(),
            backlog_bytes: self.hibernated_backlog.lock().len(),
            renderer_bytes: self.renderer.memory_usage(),
        }
    }

    pub fn with_resize_callback(
        mut self,
        callback: impl Fn(usize, usize) + Send + Sync + 'static,
    ) -> Self {
        self.resize_callback = Some(Arc::new(Box::new(callback)));
        self
    }

    /// Intercept key events before the terminal sees them. Return `true` to
    /// consume the event.
    pub fn with_key_handler(
        mut self,
        handler: impl Fn(&KeyDownEvent) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.key_handler = Some(Arc::new(Box::new(handler)));
        self
    }

    pub fn with_bell_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.bell_callback = Some(Box::new(callback));
        self
    }

    pub fn with_title_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.title_callback = Some(Box::new(callback));
        self
    }

    /// Install (or replace) the `OSC 0 / 2` title callback after construction.
    ///
    /// Hosts that draw a tab strip want the title in *their* entity: reading it
    /// back out of this view would subscribe the strip to this entity, and this
    /// entity notifies on every single PTY write.
    pub fn set_title_callback(
        &mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) {
        self.title_callback = Some(Box::new(callback));
    }

    pub fn with_clipboard_store_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.clipboard_store_callback = Some(Box::new(callback));
        self
    }

    pub fn with_exit_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.exit_callback = Some(Box::new(callback));
        self
    }

    /// Called when a hyperlink is activated (modifier-click). Return `true` to
    /// suppress the default `open_url` behaviour — use this to open
    /// `path:line:column` targets in the editor instead of a browser.
    pub fn with_link_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &HyperlinkMatch) -> bool + 'static,
    ) -> Self {
        self.link_callback = Some(Box::new(callback));
        self
    }

    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        if self.read_only != read_only {
            self.read_only = read_only;
            if read_only {
                self.blink_task = None;
                self.cursor_visible = true;
            } else {
                self.blink_restart_pending = true;
            }
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
    // -----------------------------------------------------------------------
    // PTY plumbing
    // -----------------------------------------------------------------------

    fn read_stdout_blocking<R: Read + Send + 'static>(
        mut stdout_reader: R,
        bytes_tx: flume::Sender<OutputChunk>,
        recycle_rx: flume::Receiver<Vec<u8>>,
    ) {
        loop {
            let mut buffer = recycle_rx
                .try_recv()
                .unwrap_or_else(|_| vec![0; PTY_READ_BYTES]);
            match stdout_reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let read_at = crate::diagnostics::output_received();
                    let chunk = OutputChunk {
                        buffer,
                        len: n,
                        read_at,
                    };
                    if bytes_tx.send(chunk).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// Admit one whole frame without touching the physical PTY on GPUI.
    fn write_to_pty(&mut self, bytes: &[u8]) {
        self.ensure_started();
        // Failures also go to the independently waking input-error task.
        let _ = self.stdin_writer.send(bytes);
    }

    /// Send text to the shell as if the user had typed it.
    pub fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        // Input implies the person is looking at this pane, even if the poller
        // has not seen a render yet.
        self.wake(cx);
        self.scroll_to_bottom_on_input(cx);
        self.write_to_pty(text.as_bytes());
    }

    pub fn send_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.wake(cx);
        self.scroll_to_bottom_on_input(cx);
        self.write_to_pty(bytes);
    }

    /// Send text and acknowledge only after its physical write completes.
    pub fn send_text_with_completion(
        &mut self,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Result<WriteCompletion, String> {
        self.send_bytes_with_completion(text.as_bytes(), cx)
    }

    /// Refuse an entire frame on overflow; no prefix is admitted.
    pub fn send_bytes_with_completion(
        &mut self,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) -> Result<WriteCompletion, String> {
        self.wake(cx);
        self.scroll_to_bottom_on_input(cx);
        self.ensure_started();
        self.stdin_writer.send_with_completion(bytes)
    }

    /// Surface asynchronous session/input errors even when stdout is silent.
    pub fn report_io_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.io_error = Some(error);
        cx.notify();
    }

    /// Record the real child status; reader EOF remains a distinct event.
    pub fn record_child_exit(&mut self, code: i32, cx: &mut Context<Self>) {
        self.exit_status = Some(code);
        cx.notify();
    }

    /// Disable raw command-output capture when the host only consumes markers.
    pub fn set_capture_output(&mut self, enabled: bool) {
        self.state.set_capture_output(enabled);
    }

    // -----------------------------------------------------------------------
    // Geometry helpers
    // -----------------------------------------------------------------------

    fn content_origin(&self) -> Point<Pixels> {
        let bounds = *self.last_bounds.lock();
        Point {
            x: bounds.origin.x + self.config.padding.left,
            y: bounds.origin.y + self.config.padding.top,
        }
    }

    /// Convert a mouse position into a grid point plus the side of the cell the
    /// pointer is on, which is what alacritty's selection needs to decide
    /// whether a cell is included.
    fn grid_point_and_side(
        &self,
        position: Point<Pixels>,
        display_offset: usize,
    ) -> (GridPoint, GridSide) {
        let origin = self.content_origin();
        let cell_w = f32::from(self.renderer.cell_width).max(1.0);
        let cell_h = f32::from(self.renderer.cell_height).max(1.0);

        let rel_x = f32::from(position.x - origin.x);
        let rel_y = f32::from(position.y - origin.y);

        let (cols, rows) = self.dimensions();

        let col_f = (rel_x / cell_w).floor();
        let col = (col_f.max(0.0) as usize).min(cols.saturating_sub(1));
        let row = ((rel_y / cell_h).floor().max(0.0) as usize).min(rows.saturating_sub(1));

        let side = if rel_x - col_f * cell_w > cell_w / 2.0 {
            GridSide::Right
        } else {
            GridSide::Left
        };

        let point = viewport_to_point(display_offset, GridPoint::new(row, GridColumn(col)));
        (point, side)
    }

    /// Number of lines the viewport should auto-scroll because the pointer is
    /// being dragged past the top or bottom edge.
    fn drag_scroll_lines(&self, position: Point<Pixels>) -> i32 {
        let bounds = *self.last_bounds.lock();
        let cell_h = f32::from(self.renderer.cell_height).max(1.0);

        let top = f32::from(bounds.origin.y + self.config.padding.top);
        let bottom = f32::from(bounds.origin.y + bounds.size.height - self.config.padding.bottom);
        let y = f32::from(position.y);

        if y < top {
            (((top - y) / cell_h).ceil() as i32).clamp(1, 20)
        } else if y > bottom {
            -(((y - bottom) / cell_h).ceil() as i32).clamp(1, 20)
        } else {
            0
        }
    }

    fn is_over_scrollbar(&self, position: Point<Pixels>) -> bool {
        if !self.config.show_scrollbar || self.state.history_size() == 0 {
            return false;
        }
        let bounds = *self.last_bounds.lock();
        position.x >= bounds.origin.x + bounds.size.width - SCROLLBAR_HIT_WIDTH
    }

    fn mouse_reporting_active(&self, mode: TermMode) -> bool {
        mode.intersects(
            TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG,
        )
    }

    // -----------------------------------------------------------------------
    // Scrolling
    // -----------------------------------------------------------------------

    pub fn scroll_lines(&mut self, lines: i32, cx: &mut Context<Self>) {
        if scroll_lines_if_changed(&self.state, lines) {
            cx.notify();
        }
    }

    pub fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.state.scroll_to_bottom();
        cx.notify();
    }

    pub fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.state.scroll_to_top();
        cx.notify();
    }

    pub fn page_up(&mut self, cx: &mut Context<Self>) {
        self.state.scroll(GridScroll::PageUp);
        cx.notify();
    }

    pub fn page_down(&mut self, cx: &mut Context<Self>) {
        self.state.scroll(GridScroll::PageDown);
        cx.notify();
    }

    /// Jump back to the newest output, which is what every terminal does the
    /// moment the user types something while scrolled up.
    fn scroll_to_bottom_on_input(&mut self, cx: &mut Context<Self>) {
        if !self.state.is_at_bottom() {
            self.state.scroll_to_bottom();
            cx.notify();
        }
    }

    fn scroll_to_mouse_y(&mut self, mouse_y: Pixels, cx: &mut Context<Self>) {
        let bounds = *self.last_bounds.lock();
        if bounds.size.height <= px(0.0) {
            return;
        }

        let y_rel = (mouse_y - bounds.origin.y).clamp(px(0.0), bounds.size.height);
        let fraction = f32::from(y_rel) / f32::from(bounds.size.height).max(1.0);

        let history_size = self.state.history_size();
        if history_size == 0 {
            return;
        }

        let target_offset = ((1.0 - fraction) * history_size as f32).round() as i32;
        let delta = target_offset - self.state.display_offset() as i32;
        self.scroll_lines(delta, cx);
    }

    // -----------------------------------------------------------------------
    // Selection & clipboard
    // -----------------------------------------------------------------------

    pub fn selected_text(&self) -> Option<String> {
        self.state.selection_text()
    }

    pub fn has_selection(&self) -> bool {
        self.state.has_selection()
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.state.select_all();
        cx.notify();
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.state.has_selection() {
            self.state.clear_selection();
            cx.notify();
        }
    }

    pub fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        match self.state.selection_text() {
            Some(text) if !text.is_empty() => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                cx.notify();
                true
            }
            _ => false,
        }
    }

    /// Wrap `text` in bracketed-paste markers when the application asked for
    /// them, and normalise newlines so pasting a multi-line snippet does not
    /// execute every line.
    fn paste_payload(&self, text: &str) -> Vec<u8> {
        if self.state.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        }
    }

    pub fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.scroll_to_bottom_on_input(cx);
        let payload = self.paste_payload(&text);
        self.write_to_pty(&payload);
    }

    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.paste_text_from_app(text, cx);
    }

    /// Write `text` to the PTY with paste semantics even while the view is
    /// read-only. The Shob input bar owns delivery, so agent TUIs still
    /// need bracketed-paste framing to recognize pasted image paths and attach
    /// them instead of showing the raw path.
    pub fn paste_text_from_app(&mut self, text: &str, cx: &mut Context<Self>) {
        self.scroll_to_bottom_on_input(cx);
        let payload = self.paste_payload(text);
        self.write_to_pty(&payload);
    }

    /// Paste framing is one atomic frame; the receipt covers its closing marker.
    pub fn paste_text_from_app_with_completion(
        &mut self,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Result<WriteCompletion, String> {
        self.scroll_to_bottom_on_input(cx);
        let payload = self.paste_payload(text);
        self.ensure_started();
        self.stdin_writer.send_with_completion(&payload)
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    /// Set and validate a query; highlights are published asynchronously.
    pub fn search(&mut self, query: &str, use_regex: bool, cx: &mut Context<Self>) -> bool {
        self.reset_search();
        let ok = self.search.prepare_query(query, use_regex);
        if ok && !query.is_empty() {
            self.search_select_first = true;
            self.refresh_streaming_search(cx);
        }
        cx.notify();
        ok
    }

    pub fn next_match(&mut self, cx: &mut Context<Self>) {
        if !self.search.matches().is_empty()
            && self
                .search
                .advance(GridDirection::Right, &self.state)
                .is_some()
        {
            cx.notify();
        }
    }

    pub fn previous_match(&mut self, cx: &mut Context<Self>) {
        if !self.search.matches().is_empty()
            && self
                .search
                .advance(GridDirection::Left, &self.state)
                .is_some()
        {
            cx.notify();
        }
    }

    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.reset_search();
        cx.notify();
    }

    fn reset_search(&mut self) {
        self.search.clear();
        self.search_epoch = self.search_epoch.wrapping_add(1);
        self.search_select_first = false;
        self.search_refresh_pending = false;
        self.search_last_refresh = None;
    }

    /// `(current, total)` match counters for a search UI.
    pub fn search_status(&self) -> (Option<usize>, usize) {
        (self.search.current_index(), self.search.match_count())
    }

    fn refresh_streaming_search(&mut self, cx: &mut Context<Self>) {
        self.search_refresh_pending = true;
        if self.search_refresh_task.is_some() {
            return;
        }
        let remaining = self.search_last_refresh.map_or(Duration::ZERO, |last| {
            SEARCH_REFRESH_MIN_INTERVAL.saturating_sub(last.elapsed())
        });
        self.search_refresh_task = Some(cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let executor = cx.background_executor().clone();
                if !remaining.is_zero() {
                    executor.timer(remaining).await;
                }
                let Ok(Some((mut regex, epoch, revision, columns, lines, top))) =
                    this.update(cx, |view, _| {
                        if !view.search_refresh_pending
                            || !(view.search_select_first || view.visible_recently())
                        {
                            return None;
                        }
                        let regex = view.search.take_regex()?;
                        view.search_refresh_pending = false;
                        view.search_last_refresh = Some(std::time::Instant::now());
                        let (columns, lines, top) = view.state.with_term(|term| {
                            (term.columns(), term.total_lines(), term.topmost_line().0)
                        });
                        Some((
                            regex,
                            view.search_epoch,
                            view.screen_revision(),
                            columns,
                            lines,
                            top,
                        ))
                    })
                else {
                    let _ = this.update(cx, |view, _| view.search_refresh_task = None);
                    return;
                };
                let mut snapshot = executor
                    .spawn(async move { SearchSnapshot::new(columns, lines, top) })
                    .await;
                let total = columns * lines;
                let mut complete = true;
                for start in (0..total).step_by(SEARCH_SNAPSHOT_CELLS_PER_SLICE) {
                    let copied = this.update(cx, |view, _| {
                        if epoch != view.search_epoch || revision != view.screen_revision() {
                            return false;
                        }
                        view.state.with_term(|term| {
                            snapshot.copy_cells(
                                term,
                                start,
                                (total - start).min(SEARCH_SNAPSHOT_CELLS_PER_SLICE),
                            );
                        });
                        true
                    });
                    if !matches!(copied, Ok(true)) {
                        complete = false;
                        break;
                    }
                    if total - start > SEARCH_SNAPSHOT_CELLS_PER_SLICE {
                        yield_to_ui().await;
                    }
                }
                let (regex, matches) = executor
                    .spawn(async move {
                        let matches = complete.then(|| snapshot.matches(&mut regex));
                        (regex, matches)
                    })
                    .await;
                let _ = this.update(cx, |view, cx| {
                    view.search_refresh_task = None;
                    if epoch == view.search_epoch {
                        view.search.finish_scan(regex);
                        if let Some(matches) =
                            matches.filter(|_| revision == view.screen_revision())
                        {
                            view.search.set_matches(matches);
                            if std::mem::take(&mut view.search_select_first) {
                                view.search.advance(GridDirection::Right, &view.state);
                            }
                            cx.notify();
                        } else {
                            view.search_refresh_pending = true;
                        }
                    }
                    if view.search_refresh_pending
                        && (view.search_select_first || view.visible_recently())
                    {
                        view.refresh_streaming_search(cx);
                    }
                });
            },
        ));
    }

    // -----------------------------------------------------------------------
    // Vi mode
    // -----------------------------------------------------------------------

    pub fn is_vi_mode(&self) -> bool {
        self.state.is_vi_mode()
    }

    pub fn is_alt_screen(&self) -> bool {
        self.state.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn term_mode(&self) -> TermMode {
        self.state.mode()
    }
    pub fn toggle_vi_mode(&mut self, cx: &mut Context<Self>) {
        self.state.toggle_vi_mode();
        cx.notify();
    }

    /// Handle a keystroke while vi mode is active. Returns `true` when it was
    /// consumed.
    fn handle_vi_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let key = keystroke.key.as_str();
        let shift = keystroke.modifiers.shift;
        let ctrl = keystroke.modifiers.control;

        let motion = match (key, shift) {
            // Screen positioning (`H`, `M`, `L`) before the plain movements so
            // the shifted variants win.
            ("h", true) => Some(ViMotion::High),
            ("m", true) => Some(ViMotion::Middle),
            ("l", true) => Some(ViMotion::Low),
            ("h", false) | ("left", _) => Some(ViMotion::Left),
            ("j", false) | ("down", _) => Some(ViMotion::Down),
            ("k", false) | ("up", _) => Some(ViMotion::Up),
            ("l", false) | ("right", _) => Some(ViMotion::Right),
            ("w", false) => Some(ViMotion::SemanticRight),
            ("w", true) => Some(ViMotion::WordRight),
            ("b", false) => Some(ViMotion::SemanticLeft),
            ("b", true) => Some(ViMotion::WordLeft),
            ("e", false) => Some(ViMotion::SemanticRightEnd),
            ("e", true) => Some(ViMotion::WordRightEnd),
            ("0", _) => Some(ViMotion::First),
            ("$", _) => Some(ViMotion::Last),
            ("^", _) => Some(ViMotion::FirstOccupied),
            ("%", _) => Some(ViMotion::Bracket),
            ("{", _) => Some(ViMotion::ParagraphUp),
            ("}", _) => Some(ViMotion::ParagraphDown),
            _ => None,
        };

        if let Some(motion) = motion {
            self.state.vi_motion(motion);
            cx.notify();
            return true;
        }

        match key {
            "escape" | "q" => {
                self.state.clear_selection();
                self.state.toggle_vi_mode();
                cx.notify();
                true
            }
            "g" => {
                if shift {
                    self.state.scroll_to_bottom();
                } else {
                    self.state.scroll_to_top();
                }
                cx.notify();
                true
            }
            "v" => {
                let point = self.state.vi_cursor_point();
                let ty = if ctrl {
                    GridSelectionType::Block
                } else if shift {
                    GridSelectionType::Lines
                } else {
                    GridSelectionType::Simple
                };
                self.state.start_selection(ty, point, GridSide::Left);
                cx.notify();
                true
            }
            "y" => {
                self.copy_selection(cx);
                self.state.clear_selection();
                cx.notify();
                true
            }
            "n" => {
                if shift {
                    self.previous_match(cx);
                } else {
                    self.next_match(cx);
                }
                true
            }
            "pageup" => {
                self.page_up(cx);
                true
            }
            "pagedown" => {
                self.page_down(cx);
                true
            }
            _ => false,
        }
    }

    // -----------------------------------------------------------------------
    // Hyperlinks
    // -----------------------------------------------------------------------

    fn link_modifier_held(modifiers: &Modifiers) -> bool {
        // Cmd on macOS, Ctrl elsewhere — matching Zed.
        if cfg!(target_os = "macos") {
            modifiers.platform
        } else {
            modifiers.control
        }
    }

    fn hover_revision(&self) -> HoverRevision {
        HoverRevision {
            screen: self.screen_revision(),
            display_offset: self.state.display_offset(),
        }
    }

    fn update_hovered_link(
        &mut self,
        position: Point<Pixels>,
        modifiers: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        if !Self::link_modifier_held(modifiers) {
            if self.hovered_link.take().is_some() {
                cx.notify();
            }
            return;
        }

        let revision = self.hover_revision();
        let (point, _) = self.grid_point_and_side(position, revision.display_offset);
        if self
            .hovered_link
            .as_ref()
            .is_some_and(|hovered| hovered.revision == revision && hovered.link.contains(point))
        {
            return;
        }

        let detect_paths = self.config.detect_path_links;
        let searches = self.regex_searches.get_or_insert_with(RegexSearches::new);
        let found = self
            .state
            .with_term(|term| crate::hyperlink::find_at(term, point, searches, detect_paths))
            .map(|link| HoveredLink { revision, link });

        if found != self.hovered_link {
            self.hovered_link = found;
            cx.notify();
        }
    }

    fn activate_link(&mut self, link: HyperlinkMatch, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(callback) = self.link_callback.take() {
            let handled = callback(window, cx, &link);
            self.link_callback = Some(callback);
            if handled {
                return;
            }
        }

        if link.is_url() {
            cx.open_url(&link.text);
        }
    }

    // -----------------------------------------------------------------------
    // Keyboard
    // -----------------------------------------------------------------------

    pub fn dispatch_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_key_down(event, window, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let key = keystroke.key.to_ascii_lowercase();
        let modifiers = keystroke.modifiers;

        // The cursor should be solid while the user is actively typing.
        self.cursor_visible = true;

        // --- clipboard shortcuts -------------------------------------------
        if modifiers.control && modifiers.shift && !modifiers.alt {
            match key.as_str() {
                "c" => {
                    self.copy_selection(cx);
                    return;
                }
                "v" => {
                    self.paste_from_clipboard(cx);
                    return;
                }
                "a" => {
                    self.select_all(cx);
                    return;
                }
                "f" => {
                    // Leave search UI to the embedder but make the intent
                    // observable by clearing stale highlights.
                    self.clear_search(cx);
                    return;
                }
                "space" => {
                    self.toggle_vi_mode(cx);
                    return;
                }
                _ => {}
            }
        }

        if modifiers.control && !modifiers.shift && !modifiers.alt {
            match key.as_str() {
                "c" => {
                    // Ctrl-C copies when there is a selection, otherwise it sends SIGINT.
                    if self.state.has_selection() {
                        self.copy_selection(cx);
                        self.state.clear_selection();
                        return;
                    } else {
                        self.write_to_pty(b"\x03");
                        return;
                    }
                }
                "z" => {
                    self.write_to_pty(b"\x1a");
                    return;
                }
                "\\" => {
                    self.write_to_pty(b"\x1c");
                    return;
                }
                "d" => {
                    self.write_to_pty(b"\x04");
                    return;
                }
                "l" => {
                    self.write_to_pty(b"\x0c");
                    return;
                }
                "a" => {
                    self.select_all(cx);
                    return;
                }
                _ => {}
            }
        }

        if modifiers.shift && !modifiers.control && !modifiers.alt && key == "insert" {
            self.paste_from_clipboard(cx);
            return;
        }
        if modifiers.control && !modifiers.shift && !modifiers.alt && key == "insert" {
            self.copy_selection(cx);
            return;
        }

        // --- scrollback navigation -----------------------------------------
        if !modifiers.control && !modifiers.alt {
            let scroll = match key.as_str() {
                "pageup" => Some(GridScroll::PageUp),
                "pagedown" => Some(GridScroll::PageDown),
                "home" if modifiers.shift || self.read_only => Some(GridScroll::Top),
                "end" if modifiers.shift || self.read_only => Some(GridScroll::Bottom),
                _ => None,
            };

            if let Some(scroll) = scroll {
                if modifiers.shift || self.read_only {
                    self.state.scroll(scroll);
                    cx.notify();
                    return;
                }
            }
        }

        // --- vi mode --------------------------------------------------------
        if self.state.is_vi_mode() && self.handle_vi_keystroke(keystroke, cx) {
            return;
        }

        // --- embedder hook ---------------------------------------------------
        if let Some(handler) = self.key_handler.clone() {
            if handler(event) {
                self.scroll_to_bottom_on_input(cx);
                return;
            }
        }

        // Typing anywhere dismisses the selection, like every other terminal.
        if self.state.has_selection() {
            self.state.clear_selection();
            cx.notify();
        }

        if self.read_only {
            return;
        }

        if let Some(bytes) = keystroke_to_bytes(keystroke, self.state.mode()) {
            self.scroll_to_bottom_on_input(cx);
            self.write_to_pty(&bytes);
            let cursor_was_hidden = !self.cursor_visible;
            self.restart_blink(cx);
            if cursor_was_hidden {
                cx.notify();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Mouse
    // -----------------------------------------------------------------------

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.read_only {
            window.focus(&self.focus_handle, cx);
        }
        self.last_mouse_position = event.position;

        // A host-provided context menu owns right click completely. In
        // particular, do not send a mouse report to a full-screen TUI and do
        // not perform the legacy copy/paste gesture before the event bubbles
        // to the host's menu wrapper.
        if event.button == MouseButton::Right && !self.config.right_click_paste {
            return;
        }

        if self.is_over_scrollbar(event.position) {
            self.drag_scroll_task = None;
            self.scrollbar_dragging = true;
            self.scroll_to_mouse_y(event.position.y, cx);
            return;
        }

        self.mouse_down_button = Some(event.button);

        let mode = self.state.mode();

        // Modifier-click opens the hyperlink under the pointer.
        if event.button == MouseButton::Left && Self::link_modifier_held(&event.modifiers) {
            self.update_hovered_link(event.position, &event.modifiers, cx);
            if let Some(hovered) = &self.hovered_link {
                self.activate_link(hovered.link.clone(), window, cx);
                return;
            }
        }

        // Shift bypasses application mouse reporting so the user can always
        // select text, even inside a full screen TUI.
        if self.mouse_reporting_active(mode) && !event.modifiers.shift {
            if self.read_only {
                self.clear_selection(cx);
                return;
            }
            let modifiers = crate::mouse::encode_modifiers(
                event.modifiers.shift,
                event.modifiers.alt,
                event.modifiers.control,
            );
            if let Some(report) = self.viewport_point(event.position).and_then(|vp| {
                crate::mouse::mouse_button_report(event.button, true, vp, modifiers, mode)
            }) {
                self.write_to_pty(&report);
                self.last_reported_cell = self.viewport_point(event.position);
            }
            self.clear_selection(cx);
            return;
        }

        match event.button {
            MouseButton::Left => {
                let (point, side) =
                    self.grid_point_and_side(event.position, self.state.display_offset());
                let ty = match event.click_count {
                    0 | 1 => {
                        if event.modifiers.alt {
                            GridSelectionType::Block
                        } else {
                            GridSelectionType::Simple
                        }
                    }
                    2 => GridSelectionType::Semantic,
                    _ => GridSelectionType::Lines,
                };
                self.state.start_selection(ty, point, side);
                cx.notify();
            }
            MouseButton::Middle => {
                // Middle click pastes, matching X11 convention.
                self.paste_from_clipboard(cx);
            }
            MouseButton::Right => {
                if self.state.has_selection() {
                    self.copy_selection(cx);
                    self.state.clear_selection();
                    cx.notify();
                } else {
                    self.paste_from_clipboard(cx);
                }
            }
            _ => {}
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.drag_scroll_task = None;

        if event.button == MouseButton::Right && !self.config.right_click_paste {
            self.mouse_down_button = None;
            return;
        }

        if self.scrollbar_dragging {
            self.scrollbar_dragging = false;
            cx.notify();
            return;
        }

        let mode = self.state.mode();
        if self.mouse_reporting_active(mode) && !event.modifiers.shift {
            let modifiers = crate::mouse::encode_modifiers(
                event.modifiers.shift,
                event.modifiers.alt,
                event.modifiers.control,
            );
            if let Some(report) = self.viewport_point(event.position).and_then(|vp| {
                crate::mouse::mouse_button_report(event.button, false, vp, modifiers, mode)
            }) {
                self.write_to_pty(&report);
            }
        } else if self.config.copy_on_select
            && event.button == MouseButton::Left
            && self.state.has_selection()
        {
            self.copy_selection(cx);
        }

        self.mouse_down_button = None;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.last_mouse_position = event.position;

        let hovered_scrollbar = self.is_over_scrollbar(event.position);
        if hovered_scrollbar != self.scrollbar_hovered {
            self.scrollbar_hovered = hovered_scrollbar;
            cx.notify();
        }

        if self.scrollbar_dragging {
            self.scroll_to_mouse_y(event.position.y, cx);
            return;
        }

        self.update_hovered_link(event.position, &event.modifiers, cx);

        let mode = self.state.mode();
        let motion_reporting = mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG);

        if motion_reporting && !event.modifiers.shift {
            self.drag_scroll_task = None;
            let viewport_point = self.viewport_point(event.position);
            if viewport_point != self.last_reported_cell {
                let modifiers = crate::mouse::encode_modifiers(
                    event.modifiers.shift,
                    event.modifiers.alt,
                    event.modifiers.control,
                );
                if let Some(report) = viewport_point.and_then(|vp| {
                    crate::mouse::mouse_motion_report(vp, self.mouse_down_button, modifiers, mode)
                }) {
                    self.write_to_pty(&report);
                    self.last_reported_cell = viewport_point;
                }
            }
            return;
        }

        if self.mouse_down_button == Some(MouseButton::Left) {
            if self.extend_selection_to(event.position) {
                cx.notify();
            }
            if self.drag_scroll_lines(event.position) != 0 {
                self.start_drag_scroll(cx);
                return;
            }
        }
        self.drag_scroll_task = None;
    }

    fn extend_selection_to(&self, position: Point<Pixels>) -> bool {
        let (point, side) = self.grid_point_and_side(position, self.state.display_offset());
        self.state.with_term_mut(|term| {
            term.selection
                .as_mut()
                .is_some_and(|selection| update_selection_endpoint(selection, point, side))
        })
    }

    fn start_drag_scroll(&mut self, cx: &mut Context<Self>) {
        if self.drag_scroll_task.is_some() || self.exited {
            return;
        }
        self.drag_scroll_task = Some(cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor().timer(DRAG_SCROLL_INTERVAL).await;
                    let keep_going = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                        let position = view.last_mouse_position;
                        if view.mouse_down_button == Some(MouseButton::Left)
                            && scroll_lines_if_changed(
                                &view.state,
                                view.drag_scroll_lines(position),
                            )
                        {
                            view.extend_selection_to(position);
                            cx.notify();
                            true
                        } else {
                            view.drag_scroll_task = None;
                            false
                        }
                    });
                    if !matches!(keep_going, Ok(true)) {
                        break;
                    }
                }
            },
        ));
    }

    /// Mouse position as a *viewport* relative point, which is the coordinate
    /// space mouse reports use. `None` when outside the grid.
    fn viewport_point(&self, position: Point<Pixels>) -> Option<GridPoint> {
        let origin = self.content_origin();
        let cell_w = f32::from(self.renderer.cell_width).max(1.0);
        let cell_h = f32::from(self.renderer.cell_height).max(1.0);

        let rel_x = f32::from(position.x - origin.x);
        let rel_y = f32::from(position.y - origin.y);
        if rel_x < 0.0 || rel_y < 0.0 {
            return None;
        }

        let (cols, rows) = self.dimensions();
        let col = ((rel_x / cell_w) as usize).min(cols.saturating_sub(1));
        let row = ((rel_y / cell_h) as usize).min(rows.saturating_sub(1));
        Some(GridPoint::new(GridLine(row as i32), GridColumn(col)))
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let cell_height = self.renderer.cell_height;
        let pixel_y = f32::from(event.delta.pixel_delta(cell_height).y)
            * self.config.scroll_sensitivity.max(0.05);

        if pixel_y == 0.0 {
            return;
        }

        // Reset the accumulator when the direction flips so a quick reversal
        // feels immediate rather than eating the first notch.
        if (self.scroll_accumulator > 0.0 && pixel_y < 0.0)
            || (self.scroll_accumulator < 0.0 && pixel_y > 0.0)
        {
            self.scroll_accumulator = 0.0;
        }
        self.scroll_accumulator += pixel_y;

        let cell_height_f32 = f32::from(cell_height).max(1.0);
        let mut lines = (self.scroll_accumulator / cell_height_f32) as i32;
        if lines == 0 {
            if self.scroll_accumulator >= cell_height_f32 * 0.5 {
                lines = 1;
            } else if self.scroll_accumulator <= -cell_height_f32 * 0.5 {
                lines = -1;
            } else {
                return;
            }
        }

        self.scroll_accumulator -= (lines as f32) * cell_height_f32;
        let max_lines = self.config.max_scroll_lines.max(1);
        lines = lines.clamp(-max_lines, max_lines);

        let mode = self.state.mode();
        let in_alt_screen = mode.contains(TermMode::ALT_SCREEN);
        let reporting = self.mouse_reporting_active(mode);

        if !in_alt_screen && !reporting {
            self.scroll_lines(lines, cx);
            return;
        }

        let modifiers = crate::mouse::encode_modifiers(
            event.modifiers.shift,
            event.modifiers.alt,
            event.modifiers.control,
        );
        let cell_point = self
            .viewport_point(event.position)
            .unwrap_or_else(|| GridPoint::new(GridLine(0), GridColumn(0)));

        if reporting {
            if let Some(report) = crate::mouse::scroll_report(lines, cell_point, modifiers, mode) {
                self.write_to_pty(&report);
                return;
            }
        }

        if in_alt_screen
            && self.config.alternate_scroll
            && mode.contains(TermMode::ALTERNATE_SCROLL)
        {
            if let Some(report) = crate::mouse::scroll_report(lines, cell_point, modifiers, mode) {
                self.write_to_pty(&report);
                return;
            }
        }

        self.scroll_lines(lines, cx);
    }

    // -----------------------------------------------------------------------
    // Cursor blink & focus
    // -----------------------------------------------------------------------

    fn can_blink(&self) -> bool {
        !self.read_only
            && !self.exited
            && self.config.cursor_blink
            && self.was_focused
            && self.state.cursor_blinking()
    }

    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_restart_pending = false;
        self.cursor_visible = true;

        // Only animate when the terminal is focused *and* the application
        // actually asked for a blinking cursor — otherwise we would repaint
        // twice a second for nothing.
        if !self.visible_recently() || !self.can_blink() {
            self.blink_task = None;
            return;
        }

        let interval = self.config.blink_interval;
        self.blink_task = Some(
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor().timer(interval).await;
                    // The cursor may have gone steady (or the view unfocused)
                    // since the task started; stop then instead of repainting
                    // twice a second for nothing. `restart_blink` relaunches us
                    // when blinking is wanted again.
                    let mut keep_going = false;
                    let alive = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                        if view.can_blink() {
                            view.cursor_visible = !view.cursor_visible;
                            cx.notify();
                            keep_going = true;
                        } else {
                            view.cursor_visible = true;
                            view.blink_task = None;
                        }
                    });
                    if alive.is_err() || !keep_going {
                        break;
                    }
                }
            }),
        );
    }

    /// Emit `CSI I` / `CSI O` when the application enabled focus reporting
    /// (DEC mode 1004). Editors like vim use it to refresh buffers.
    fn report_focus_change(&mut self, focused: bool) {
        if !self.state.mode().contains(TermMode::FOCUS_IN_OUT) {
            return;
        }
        self.write_to_pty(if focused { b"\x1b[I" } else { b"\x1b[O" });
    }

    // -----------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------

    fn process_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(event) = self.deferred_events.pop() {
            self.process_window_event(event, true, window, cx);
        }
        while let Ok(event) = self.event_rx.try_recv() {
            self.process_window_event(event, false, window, cx);
        }
    }

    fn process_window_event(
        &mut self,
        event: TerminalEvent,
        deferred: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalEvent::Wakeup | TerminalEvent::MouseCursorDirty => {}
            TerminalEvent::Bell => {
                if let Some(callback) = self.bell_callback.take() {
                    callback(window, cx);
                    self.bell_callback = Some(callback);
                }
            }
            TerminalEvent::Title(title) => {
                if let Some(callback) = self.title_callback.take() {
                    callback(window, cx, &title);
                    self.title_callback = Some(callback);
                }
                if !deferred {
                    self.title = Some(title);
                }
            }
            TerminalEvent::ResetTitle => {
                if !deferred {
                    self.title = None;
                }
                if let Some(callback) = self.title_callback.take() {
                    callback(window, cx, "");
                    self.title_callback = Some(callback);
                }
            }
            TerminalEvent::ClipboardStore(_, text) => {
                if let Some(callback) = self.clipboard_store_callback.take() {
                    callback(window, cx, &text);
                    self.clipboard_store_callback = Some(callback);
                }
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            TerminalEvent::ClipboardLoad(_, formatter) => {
                if self.config.allow_osc52_paste {
                    let text = cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .unwrap_or_default();
                    let payload = formatter(&text);
                    self.write_to_pty(payload.as_bytes());
                }
            }
            TerminalEvent::CursorBlinkingChange => self.blink_restart_pending = true,
            TerminalEvent::ChildExit(code) => self.exit_status = Some(code),
            TerminalEvent::Exit => {
                self.mark_exited();
                // Observation may have been in the background; the callback
                // is consumed only here, with a window, and cannot run twice.
                if let Some(callback) = self.exit_callback.take() {
                    callback(window, cx);
                }
            }
        }
    }

    fn mark_exited(&mut self) -> bool {
        if self.exited {
            return false;
        }
        self.exited = true;
        self.blink_task = None;
        self.cursor_visible = true;
        self.drag_scroll_task = None;
        true
    }

    /// Update background-readable state now and replay bounded window effects
    /// before newly received events on the next render.
    pub fn drain_background_events(&mut self, cx: &mut Context<Self>) {
        let mut state_changed = false;
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                TerminalEvent::Title(title) => {
                    if title.len() <= MAX_DEFERRED_WINDOW_TEXT_BYTES {
                        self.deferred_events
                            .push(TerminalEvent::Title(title.clone()));
                    }
                    self.title = Some(title);
                    state_changed = true;
                }
                TerminalEvent::ResetTitle => {
                    self.title = None;
                    self.deferred_events.push(TerminalEvent::ResetTitle);
                    state_changed = true;
                }
                TerminalEvent::ChildExit(code) => {
                    self.exit_status = Some(code);
                    state_changed = true;
                }
                TerminalEvent::Exit => {
                    if self.mark_exited() {
                        self.deferred_events.push(TerminalEvent::Exit);
                        state_changed = true;
                    }
                }
                TerminalEvent::CursorBlinkingChange => self.blink_restart_pending = true,
                event @ (TerminalEvent::Bell
                | TerminalEvent::ClipboardStore(..)
                | TerminalEvent::ClipboardLoad(..)) => self.deferred_events.push(event),
                TerminalEvent::Wakeup | TerminalEvent::MouseCursorDirty => {}
            }
        }
        if state_changed {
            cx.notify();
        }
    }

    // -----------------------------------------------------------------------
    // Accessors / configuration
    // -----------------------------------------------------------------------

    /// Live grid size, in cells. Read from the grid rather than tracked here,
    /// because the paint pass resizes the grid and a copy would lag it.
    pub fn dimensions(&self) -> (usize, usize) {
        self.state.dimensions()
    }

    pub fn last_bounds(&self) -> Bounds<Pixels> {
        *self.last_bounds.lock()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.state.resize(cols, rows);
        self.screen_revision.fetch_add(1, Ordering::Relaxed);
        self.sync_window_size();
        // The paint pass does this inline. Block mode hides the grid, so the
        // host calls `resize` explicitly and the PTY must follow.
        if let Some(callback) = self.resize_callback.as_ref() {
            callback(cols, rows);
        }
    }

    /// Monotonic change counter for the terminal's visible grid.
    pub fn screen_revision(&self) -> u64 {
        self.screen_revision.load(Ordering::Relaxed)
    }

    pub fn config(&self) -> &TerminalConfig {
        &self.config
    }

    /// Shaped cell size. Used by the block renderer to align captured output.
    pub fn cell_metrics(&self) -> (Pixels, Pixels) {
        (self.renderer.cell_width, self.renderer.cell_height)
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// `true` when this view has painted recently, i.e. it is on screen. A
    /// background terminal keeps parsing its output but does not spend UI time
    /// rescanning the scrollback for a search nobody can see.
    fn visible_recently(&self) -> bool {
        self.last_render
            .is_some_and(|at| at.elapsed() < SEARCH_VISIBLE_WINDOW)
    }

    /// Title reported by the application through `OSC 0 / 2`.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn set_title(&mut self, title: Option<String>) {
        self.title = title;
    }

    /// Highest-priority status explicitly reported by a process in this pane.
    pub fn agent_status_report(&self) -> Option<AgentStatusReport> {
        self.state.agent_status_report()
    }

    pub fn clear_agent_status_reports(&mut self) {
        self.state.clear_agent_status_reports();
    }

    /// Current OSC 9;4 progress value for agent-specific detection rules.
    pub fn osc_progress(&self) -> &str {
        self.state.osc_progress()
    }

    /// Most recent cwd reported by shell integration (OSC 7 / OSC 7502).
    pub fn shell_cwd(&self) -> Option<&str> {
        self.state.shell_cwd()
    }

    /// Read bottom N lines of visible terminal text
    pub fn bottom_lines(&self, n: usize) -> Vec<String> {
        self.state.bottom_lines(n)
    }

    /// Snapshot the whole buffer (scrollback + visible screen) as plain text,
    /// keeping the newest `max_lines` lines.
    ///
    /// A hibernated view has no grid history, but its kept text plus buffered
    /// output *is* its history; reporting the empty grid here would lose it on
    /// quit.
    pub fn buffer_text(&self, max_lines: usize) -> String {
        if self.hibernated.load(Ordering::Acquire) {
            let mut text = self.hibernated_text.clone();
            {
                let backlog = self.hibernated_backlog.lock();
                if !backlog.is_empty() {
                    text.push_str(&String::from_utf8_lossy(&backlog[..]));
                }
            }
            return tail_lines(&text, max_lines);
        }
        self.state.buffer_text(max_lines)
    }

    /// Like [`Self::bottom_lines`], but scrapes into a caller-provided buffer
    /// so repeated polling reuses the allocation.
    pub fn bottom_lines_into(&self, n: usize, out: &mut Vec<String>) {
        self.state.bottom_lines_into(n, out)
    }
    /// `true` once the child process is gone.
    pub fn has_exited(&self) -> bool {
        self.exited
    }

    pub fn exit_status(&self) -> Option<i32> {
        self.exit_status
    }

    /// Clear the screen *and* the scrollback.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.state.clear_screen_and_scrollback();
        self.screen_revision.fetch_add(1, Ordering::Relaxed);
        self.reset_search();
        cx.notify();
    }

    pub fn update_config(&mut self, config: TerminalConfig, cx: &mut Context<Self>) {
        let metrics_changed = self.renderer.font_family.as_str() != config.font_family
            || self.renderer.font_size != config.font_size
            || self.renderer.line_height_multiplier != config.line_height_multiplier;

        let alac_changed = self.config.scrollback != config.scrollback
            || self.config.semantic_escape_chars != config.semantic_escape_chars
            || self.config.allow_osc52_paste != config.allow_osc52_paste;

        let renderer = std::rc::Rc::make_mut(&mut self.renderer);
        renderer.font_family = config.font_family.clone().into();
        renderer.font_size = config.font_size;
        renderer.line_height_multiplier = config.line_height_multiplier;
        renderer.palette = config.colors.clone();
        self.screen_revision.fetch_add(1, Ordering::Relaxed);
        // Applications may query OSC 11 immediately after the palette report.
        *self.palette_handle.lock() = config.colors.clone();
        self.state.set_palette(config.colors.clone());
        self.state.set_shell_token(config.shell_token.clone());

        if alac_changed {
            // A hibernated grid must not start retaining history again just
            // because fonts or colours changed; wake restores the configured
            // size from `self.config`.
            let history = if self.hibernated.load(Ordering::Acquire) {
                HIBERNATED_GRID_HISTORY
            } else {
                config.scrollback
            };
            self.state.set_config(TerminalState::build_config(
                history,
                &config.semantic_escape_chars,
                config.allow_osc52_paste,
            ));
            if self.search.is_active() {
                self.search.invalidate_matches();
                self.refresh_streaming_search(cx);
            }
        }

        self.config = config;

        if metrics_changed {
            self.cell_metrics_valid = false;
        }

        self.restart_blink(cx);
        cx.notify();
    }

    fn sync_window_size(&self) {
        let (cols, rows) = self.dimensions();
        *self.window_size.lock() = PtyWindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: f32::from(self.renderer.cell_width).max(1.0) as u16,
            cell_height: f32::from(self.renderer.cell_height).max(1.0) as u16,
        };
    }
}

impl EventEmitter<ShellEvent> for TerminalView {}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A render is proof the pane is visible, so it is the wake point. Do
        // not notify: this frame is already being built.
        if self.hibernated.load(Ordering::Acquire) {
            self.wake_inner();
        }
        self.ensure_started();
        self.process_events(window, cx);

        // Hidden views and resized grids need fresh grid-coordinate matches.
        // Combine these invalidations with an owed streaming rescan so a first
        // render/resize never scans the full scrollback twice in one frame.
        let now = std::time::Instant::now();
        let was_hidden = !self.visible_recently();
        self.last_render = Some(now);
        let grid_size = self.dimensions();
        let grid_changed = grid_size != self.last_grid_size;
        self.last_grid_size = grid_size;
        if self.search.is_active() && (was_hidden || grid_changed) {
            self.search.invalidate_matches();
            self.refresh_streaming_search(cx);
        }
        if self
            .hovered_link
            .as_ref()
            .is_some_and(|hovered| hovered.revision != self.hover_revision())
        {
            self.hovered_link = None;
        }

        if !self.cell_metrics_valid {
            std::rc::Rc::make_mut(&mut self.renderer).measure_cell(window);
            self.cell_metrics_valid = true;
        }

        let is_focused = self.focus_handle.is_focused(window);
        let focus_changed = is_focused != self.was_focused;
        if focus_changed {
            self.was_focused = is_focused;
            self.state.set_focused(is_focused);
            self.report_focus_change(is_focused);
        }
        if focus_changed || self.blink_restart_pending || (was_hidden && self.blink_task.is_none())
        {
            self.restart_blink(cx);
        }

        let state_arc = self.state.term_arc();
        let renderer = self.renderer.clone();
        let resize_callback = self.resize_callback.clone();
        let padding = self.config.padding;
        let last_bounds = self.last_bounds.clone();
        let window_size = self.window_size.clone();
        let screen_revision = Arc::clone(&self.screen_revision);
        let pending_output = Arc::clone(&self.pending_output);
        let entity_id = cx.entity_id();

        let selection: Option<SelectionRange> = self.state.selection_range();
        let search_matches: Arc<[GridMatch]> = self.search.matches_arc();
        let active_match_index = self.search.current_index();
        let hovered_link = self
            .hovered_link
            .as_ref()
            .map(|hovered| hovered.link.range.clone());
        let cursor_visible = self.cursor_visible && !self.read_only;
        let show_scrollbar = self.config.show_scrollbar;
        let scrollbar_active = self.scrollbar_dragging || self.scrollbar_hovered;
        let background = self.config.colors.background();

        let selection_color = self.config.selection_color;
        let match_color = self.config.search_match_color;
        let active_match_color = self.config.active_search_match_color;

        let cursor_style = if self.hovered_link.is_some() {
            CursorStyle::PointingHand
        } else {
            CursorStyle::IBeam
        };

        div()
            .relative()
            .size_full()
            .bg(background)
            .cursor(cursor_style)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, _window, _cx| bounds,
                    move |bounds, _, window, cx| {
                        *last_bounds.lock() = bounds;

                        let available_width =
                            f32::from(bounds.size.width - padding.left - padding.right);
                        let available_height =
                            f32::from(bounds.size.height - padding.top - padding.bottom);
                        let cell_width_f32 = f32::from(renderer.cell_width).max(1.0);
                        let cell_height_f32 = f32::from(renderer.cell_height).max(1.0);

                        let cols = ((available_width / cell_width_f32) as usize).max(2);
                        let rows = ((available_height / cell_height_f32) as usize).max(1);

                        let mut term = state_arc.lock();
                        let current_cols = term.columns();
                        let current_rows = term.screen_lines();
                        let resized = cols != current_cols || rows != current_rows;
                        if resized {
                            term.resize(crate::terminal::TermDimensions::new(cols, rows));
                            screen_revision.fetch_add(1, Ordering::Relaxed);
                            drop(term);
                            *window_size.lock() = PtyWindowSize {
                                num_lines: rows as u16,
                                num_cols: cols as u16,
                                cell_width: cell_width_f32 as u16,
                                cell_height: cell_height_f32 as u16,
                            };
                            if let Some(ref callback) = resize_callback {
                                callback(cols, rows);
                            }
                            window.on_next_frame(move |_, cx| cx.notify(entity_id));
                            term = state_arc.lock();
                        }

                        let paint_ctx = PaintContext {
                            selection: if resized { None } else { selection },
                            search_matches: (!resized).then_some(&search_matches),
                            screen_revision: Some(screen_revision.load(Ordering::Relaxed)),
                            active_match: if resized {
                                None
                            } else {
                                active_match_index.and_then(|index| search_matches.get(index))
                            },
                            hovered_link: if resized { None } else { hovered_link.as_ref() },
                            is_focused,
                            cursor_visible,
                            show_scrollbar,
                            scrollbar_active,
                            selection_color,
                            match_color,
                            active_match_color,
                        };

                        renderer.paint(bounds, padding, &term, &paint_ctx, window, cx);
                        crate::diagnostics::output_painted(&pending_output);
                    },
                )
                .size_full(),
            )
            .when_some(self.io_error.clone(), |view, error| {
                view.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .px(px(8.0))
                        .py(px(4.0))
                        .bg(rgb(0x602020))
                        .text_color(rgb(0xffffff))
                        .text_size(px(12.0))
                        .child(error),
                )
            })
    }
}

#[cfg(test)]
mod hibernation_tests {
    // `use super::*` here re-globs this module's `use gpui::*`, which blows the
    // crate's macro recursion limit.
    use super::{MAX_HIBERNATED_BACKLOG_BYTES, append_bounded, tail_lines, trim_front_to_bytes};

    #[test]
    fn backlog_keeps_small_writes_verbatim() {
        let mut backlog = Vec::new();
        append_bounded(&mut backlog, b"one\ntwo\n");
        append_bounded(&mut backlog, b"three\n");
        assert_eq!(backlog.as_slice(), b"one\ntwo\nthree\n".as_slice());
    }

    #[test]
    fn backlog_is_bounded_and_trimmed_at_a_line_start() {
        let mut backlog = Vec::new();
        let line = vec![b'x'; 4096];
        let mut chunk = Vec::with_capacity(line.len() * 100);
        for _ in 0..100 {
            chunk.extend_from_slice(&line);
            chunk.push(b'\n');
        }
        append_bounded(&mut backlog, &chunk);
        assert!(backlog.len() <= MAX_HIBERNATED_BACKLOG_BYTES);
        assert!(backlog.len() >= MAX_HIBERNATED_BACKLOG_BYTES - line.len() - 1);
        // Trimming starts at a newline, so the replay does not begin inside a
        // partial line or escape sequence.
        assert_eq!(backlog.first(), Some(&b'x'));
        assert_eq!(backlog.last(), Some(&b'\n'));
    }

    #[test]
    fn backlog_of_one_unbroken_line_stays_bounded() {
        let mut backlog = Vec::new();
        append_bounded(
            &mut backlog,
            &vec![b'y'; MAX_HIBERNATED_BACKLOG_BYTES + 1024],
        );
        assert_eq!(backlog.len(), MAX_HIBERNATED_BACKLOG_BYTES);
        assert!(backlog.iter().all(|&byte| byte == b'y'));
    }

    #[test]
    fn text_trim_keeps_utf8_valid_and_the_newest_bytes() {
        let mut text = "ab\nééé\nzz".to_string();
        trim_front_to_bytes(&mut text, 3);
        assert_eq!(text, "\nzz");
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());

        let mut text = "short".to_string();
        trim_front_to_bytes(&mut text, 100);
        assert_eq!(text, "short");
    }

    #[test]
    fn tail_lines_returns_the_newest_complete_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail_lines("a\nb\nc\n", 10), "a\nb\nc\n");
        assert_eq!(tail_lines("a\nb\nc\n", 0), "");
        assert_eq!(tail_lines("no newline", 1), "no newline");
    }
}

#[cfg(test)]
mod terminal_interaction_tests {
    use super::{
        DeferredWindowEvents, GpuiEventProxy, GridColumn, GridLine, GridPoint, GridSelection,
        GridSelectionType, GridSide, MAX_DEFERRED_WINDOW_EVENTS, MAX_DEFERRED_WINDOW_TEXT_BYTES,
        TerminalEvent, TerminalState, mpsc, scroll_lines_if_changed, update_selection_endpoint,
    };

    #[test]
    fn deferred_fifo_evicts_oldest_without_losing_exit() {
        let mut queue = DeferredWindowEvents::default();
        queue.push(TerminalEvent::Bell);
        queue.push(TerminalEvent::Exit);
        for index in 0..MAX_DEFERRED_WINDOW_EVENTS {
            queue.push(TerminalEvent::Title(index.to_string()));
        }
        assert!(matches!(queue.pop(), Some(TerminalEvent::Exit)));
        for index in 1..MAX_DEFERRED_WINDOW_EVENTS {
            assert!(
                matches!(queue.pop(), Some(TerminalEvent::Title(title)) if title == index.to_string())
            );
        }
        assert!(queue.pop().is_none());
    }

    #[test]
    fn deferred_fifo_bounds_text_and_does_not_evict_for_oversized_event() {
        let mut queue = DeferredWindowEvents::default();
        queue.push(TerminalEvent::Exit);
        queue.push(TerminalEvent::Title(
            "x".repeat(MAX_DEFERRED_WINDOW_TEXT_BYTES),
        ));
        queue.push(TerminalEvent::Title(
            "y".repeat(MAX_DEFERRED_WINDOW_TEXT_BYTES + 1),
        ));
        assert_eq!(queue.text_bytes, MAX_DEFERRED_WINDOW_TEXT_BYTES);
        queue.push(TerminalEvent::Title(String::from("latest")));
        queue.push(TerminalEvent::ResetTitle);
        assert!(matches!(queue.pop(), Some(TerminalEvent::Exit)));
        assert!(matches!(queue.pop(), Some(TerminalEvent::Title(title)) if title == "latest"));
        assert!(matches!(queue.pop(), Some(TerminalEvent::ResetTitle)));
        assert!(queue.pop().is_none());
        assert_eq!(queue.text_bytes, 0);
    }

    #[test]
    fn same_selection_endpoint_is_unchanged_but_crossing_cell_side_is_not() {
        let point = GridPoint::new(GridLine(0), GridColumn(0));
        let mut selection = GridSelection::new(GridSelectionType::Simple, point, GridSide::Left);
        assert!(!update_selection_endpoint(
            &mut selection,
            point,
            GridSide::Left
        ));
        assert!(update_selection_endpoint(
            &mut selection,
            point,
            GridSide::Right
        ));
        assert!(!update_selection_endpoint(
            &mut selection,
            point,
            GridSide::Right
        ));
    }

    #[test]
    fn clamped_scroll_does_not_emit_mouse_cursor_dirty() {
        let (tx, rx) = mpsc::channel();
        let mut state = TerminalState::new(5, 2, GpuiEventProxy::new(tx));
        state.feed_raw(b"one\r\ntwo\r\nthree\r\n");
        assert!(state.history_size() > 0);
        while rx.try_recv().is_ok() {}
        assert!(!scroll_lines_if_changed(&state, -1));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(scroll_lines_if_changed(&state, state.history_size() as i32));
        assert!(matches!(rx.try_recv(), Ok(TerminalEvent::MouseCursorDirty)));
        assert!(!scroll_lines_if_changed(&state, 1));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }
}
