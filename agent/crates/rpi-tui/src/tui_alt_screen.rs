//! TuiAltScreen - Alternate screen TUI with constrained layout.
//!
//! Uses the alternate screen buffer for fullscreen mode with a scrollable
//! transcript and fixed bottom dock, as described in `tui-plan.md`.

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::component::Component;
use super::container::Container;
use super::layout::{
    composite_tui_line, extract_cursor_position, render_layout_frame,
    render_layout_frame_reusing_scroll_content, LayoutFrame,
};
use super::overlay::OverlayManager;
use super::scroll_view::ScrollView;
use super::tui::{OverlayHandle, OverlayOptions, TuiMode, TuiStopOptions, TUI};
use crate::ansi::{visible_width, CURSOR_MARKER};
use crate::terminal::{InputEvent, Terminal, TerminalInfo};
use crate::terminal_image::CellDimensions;

/// Symbol for ViewportTUI capability check.
pub const VIEWPORT_TUI: &[u8] = b"rpi-tui/viewport";

/// Upper bound on the coalesced repaint rate, matching upstream's
/// `TuiBase.MIN_RENDER_INTERVAL_MS` (16 ms ≈ 60 fps).
///
/// A provider streams one `MessageUpdate` per delta, and every delta used to
/// repaint the *whole* transcript (full markdown rebuild + full layout pass +
/// a full-viewport terminal write). Throttling that to one frame per interval
/// is what keeps long, fast output from saturating the terminal — and the
/// GPU-backed compositor behind it.
pub const MIN_RENDER_INTERVAL_MS: u64 = 16;

/// Merge an incoming frame request into one that is already parked.
///
/// `reuse_scroll_content = false` means "the transcript changed, re-render it";
/// `true` means "only the viewport/dock moved, the cached transcript is fine".
/// A parked full render is never downgraded to a cached one, so a streamed
/// delta that lands inside a pure-scroll frame window is still painted.
fn merge_pending_render(parked: Option<bool>, reuse_scroll_content: bool) -> Option<bool> {
    match parked {
        Some(parked) => Some(parked && reuse_scroll_content),
        None => Some(reuse_scroll_content),
    }
}

/// Ask one terminal for its cell size in pixels.
///
/// The answer comes from the terminal object rather than the process-wide
/// capability cache, so a renderer's geometry belongs to the terminal it is
/// drawing to. `None` means "this terminal cannot report pixels" (Windows, or
/// a Unix terminal that leaves the ioctl fields at zero).
fn poll_cell_dimensions_of(terminal: &dyn Terminal) -> Option<CellDimensions> {
    crate::terminal_image::query_cell_dimensions(
        terminal.window_size(),
        Some((terminal.columns() as u16, terminal.rows() as u16)),
    )
}

/// Coalescing render-scheduler state — the Rust port of upstream's
/// `TuiBase.{renderRequested, renderTimer, lastRenderAt}` trio.
struct RenderSchedulerState {
    /// `None` = no frame is owed. `Some(reuse_scroll_content)` records which
    /// render variant the parked request needs.
    pending: Option<bool>,
    /// Set by [`TuiAltScreen::stop`] to end the scheduler thread.
    shutdown: bool,
    /// When the last frame was painted (upstream `lastRenderAt`).
    last_render_at: Instant,
}

/// One scheduler per TUI (never global), so a parked frame can only ever wake
/// the thread that belongs to its own screen.
struct RenderScheduler {
    state: Mutex<RenderSchedulerState>,
    cv: Condvar,
}

impl RenderScheduler {
    fn new() -> Self {
        Self {
            state: Mutex::new(RenderSchedulerState {
                pending: None,
                shutdown: false,
                last_render_at: Instant::now(),
            }),
            cv: Condvar::new(),
        }
    }

    /// Record that a frame is owed and wake the scheduler thread.
    fn park(&self, reuse_scroll_content: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.pending = merge_pending_render(state.pending, reuse_scroll_content);
        }
        // The predicate is checked under the same mutex the waiter holds, so the
        // wakeup cannot be lost between the store above and the wait below.
        self.cv.notify_all();
    }

    /// Note that a frame was just painted, restarting the throttle window.
    fn mark_painted(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.last_render_at = Instant::now();
        }
    }

    /// Take the parked frame, if any.
    fn take_pending(&self) -> Option<bool> {
        self.state.lock().ok()?.pending.take()
    }

    /// Discard any parked frame (an immediate render supersedes it).
    fn clear_pending(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.pending = None;
        }
    }

    /// Signal the scheduler thread to exit and reclaim the parked frame.
    fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.pending = None;
        }
        self.cv.notify_all();
    }
}

/// Alternate screen TUI with application-owned scrolling.
pub struct TuiAltScreen {
    /// Serializes the complete diff/render/write transaction. Input, streaming
    /// events, and loader ticks can request frames from different threads; if
    /// they race, an older frame can otherwise overwrite `previous_screen`
    /// after a newer frame has already reached the terminal.
    render_lock: Mutex<()>,
    jump_to_end: crate::jump_to_end::JumpToEnd,
    terminal: Arc<Mutex<Box<dyn Terminal>>>,
    terminal_proxy: TerminalProxy,
    container: Container,
    layout_root: Mutex<Option<Arc<dyn Component>>>,
    running: Arc<Mutex<bool>>,
    show_hardware_cursor: Mutex<bool>,
    clear_on_shrink: Mutex<bool>,
    focused: Mutex<Option<Arc<dyn Component>>>,
    full_redraw_count: Mutex<usize>,
    previous_screen: Mutex<Vec<String>>,
    /// Size the frame in `previous_screen` was laid out for. Any mismatch means
    /// the terminal reflowed its rows, so the frame cannot be patched cell by
    /// cell and must be repainted whole.
    painted_size: Mutex<(usize, usize)>,
    /// Cell size in pixels the frame in `previous_screen` was laid out for.
    ///
    /// Image placements derive their row count from the cell geometry, so a
    /// frame built for another geometry cannot be diffed against: the rows that
    /// look unchanged still hold the previous geometry. Tracked separately from
    /// `painted_size` because the character grid can stay identical while the
    /// pixels per cell change (font zoom, another monitor's DPI).
    painted_cells: Mutex<CellDimensions>,
    /// The cell geometry this renderer last polled, or `None` until it polls.
    ///
    /// The polled value lives on the instance rather than only in the
    /// process-wide capability cache so that a renderer always compares and
    /// lays out against geometry it read itself. Tests then isolate by
    /// construction: a fake terminal reports its own pixel size and nothing
    /// another test does can move this renderer's geometry between two frames.
    cell_geometry: Mutex<Option<CellDimensions>>,
    /// Set when a resize event reaches the loop. The window can end up at the
    /// size it already had (dragged out and back) while every intermediate
    /// resize reflowed the terminal's rows, so the next frame repaints whole.
    resize_dirty: AtomicBool,
    scroll_top: Mutex<usize>,
    stick_to_bottom: Mutex<bool>,
    /// Share the immutable painted frame with input without cloning its layout
    /// tree and visible strings for every mouse event.
    current_frame: Mutex<Option<Arc<LayoutFrame>>>,
    /// When true, render into the terminal's main buffer and let the terminal
    /// own scrollback (upstream's default `regular` mode).
    main_screen_mode: Mutex<bool>,
    main_hardware_row: Mutex<usize>,
    main_viewport_top: Mutex<usize>,
    main_previous_cursor: Mutex<Option<(usize, usize)>>,
    /// Force live rows to repaint without forgetting the saved document or
    /// the physical cursor used for the next relative movement.
    main_force_repaint: AtomicBool,
    // Input handlers
    #[allow(dead_code)]
    input_handler: Mutex<Option<Arc<dyn Fn(InputEvent) + Send + Sync>>>,
    #[allow(dead_code)]
    resize_handler: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    overlays: Arc<OverlayManager>,
    render_suspended: Mutex<bool>,
    /// Coalescing render scheduler (upstream `TuiBase` frame throttle). While
    /// it is active, `request_render(false)` and
    /// `request_render_reusing_scroll_content()` park a frame instead of
    /// repainting, and the scheduler thread paints at most one frame per
    /// [`MIN_RENDER_INTERVAL_MS`].
    scheduler: Arc<RenderScheduler>,
    /// Whether [`TuiAltScreen::start_render_scheduler`] succeeded. When false,
    /// render requests paint inline (standalone/unit-test behaviour).
    scheduler_started: AtomicBool,
    /// The scheduler thread, joined by [`TuiAltScreen::stop`] so a parked frame
    /// cannot repaint the terminal after shutdown.
    scheduler_thread: Mutex<Option<JoinHandle<()>>>,
    /// Fullscreen text selection (anchor/focus in content coordinates).
    selection: crate::selection::SelectionState,
    /// Image payloads this screen's terminal is known to hold. Terminal state,
    /// so it belongs to the screen that owns the terminal — never a process
    /// global, which a second screen, a reconnect, or a terminal restart would
    /// silently invalidate.
    kitty_uploads: Mutex<crate::terminal_image::KittyUploads>,
}

impl TuiAltScreen {
    /// Create a new alternate screen TUI.
    pub fn new(
        terminal: Box<dyn Terminal>,
        show_hardware_cursor: bool,
        _log_directory: Option<&str>,
    ) -> Self {
        let terminal = Arc::new(Mutex::new(terminal));
        let terminal_proxy = TerminalProxy {
            terminal: terminal.clone(),
        };
        Self {
            render_lock: Mutex::new(()),
            jump_to_end: crate::jump_to_end::JumpToEnd::default(),
            terminal,
            terminal_proxy,
            container: Container::new(),
            layout_root: Mutex::new(None),
            running: Arc::new(Mutex::new(false)),
            show_hardware_cursor: Mutex::new(show_hardware_cursor),
            clear_on_shrink: Mutex::new(false),
            focused: Mutex::new(None),
            full_redraw_count: Mutex::new(0),
            previous_screen: Mutex::new(Vec::new()),
            painted_size: Mutex::new((0, 0)),
            painted_cells: Mutex::new(crate::terminal_image::get_cell_dimensions()),
            cell_geometry: Mutex::new(None),
            resize_dirty: AtomicBool::new(false),
            scroll_top: Mutex::new(0),
            stick_to_bottom: Mutex::new(true),
            current_frame: Mutex::new(None),
            main_screen_mode: Mutex::new(false),
            main_hardware_row: Mutex::new(0),
            main_viewport_top: Mutex::new(0),
            main_previous_cursor: Mutex::new(None),
            main_force_repaint: AtomicBool::new(false),
            input_handler: Mutex::new(None),
            resize_handler: Mutex::new(None),
            overlays: Arc::new(OverlayManager::new()),
            render_suspended: Mutex::new(false),
            scheduler: Arc::new(RenderScheduler::new()),
            scheduler_started: AtomicBool::new(false),
            scheduler_thread: Mutex::new(None),
            selection: crate::selection::SelectionState::new(),
            kitty_uploads: Mutex::new(crate::terminal_image::KittyUploads::new()),
        }
    }

    /// Start the coalescing render scheduler — the Rust port of upstream's
    /// `TuiBase.scheduleRender()` timer. Idempotent.
    ///
    /// Every `request_render(false)` / `request_render_reusing_scroll_content()`
    /// call after this parks a frame instead of repainting immediately, and the
    /// scheduler thread paints at most one frame per [`MIN_RENDER_INTERVAL_MS`].
    /// A burst of streamed deltas therefore collapses into a single repaint of
    /// the transcript instead of one per token.
    ///
    /// Hosts that drive their own event loop may instead leave this off and call
    /// [`TuiAltScreen::flush_pending_render`] from their own tick; without either
    /// the requests paint inline (the pre-throttle behaviour).
    ///
    /// Takes `&Arc<Self>` because the scheduler thread keeps the TUI alive until
    /// [`TuiAltScreen::stop`] shuts it down; call it after the TUI has been
    /// wrapped in the `Arc` the host renders through.
    pub fn start_render_scheduler(self: &Arc<Self>) {
        if self.scheduler_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let tui = self.clone();
        let spawned = std::thread::Builder::new()
            .name("rpi-tui-render".to_string())
            .spawn(move || tui.run_render_scheduler());
        match spawned {
            Ok(handle) => {
                if let Ok(mut slot) = self.scheduler_thread.lock() {
                    *slot = Some(handle);
                }
            }
            Err(_) => {
                // A failed spawn must not silently disable repaints: fall back
                // to the synchronous path rather than parking frames forever.
                self.scheduler_started.store(false, Ordering::Release);
            }
        }
    }

    /// Scheduler thread body: sleep until a frame is owed, then paint at most one
    /// frame per [`MIN_RENDER_INTERVAL_MS`]. Idle screens pay no wakeups at all —
    /// the thread is parked on the condvar.
    fn run_render_scheduler(self: Arc<Self>) {
        let interval = Duration::from_millis(MIN_RENDER_INTERVAL_MS);
        loop {
            // ---- Wait for a frame to be owed (or shutdown) ----
            {
                let Ok(mut state) = self.scheduler.state.lock() else {
                    return;
                };
                while state.pending.is_none() && !state.shutdown {
                    let Ok(next) = self.scheduler.cv.wait(state) else {
                        return;
                    };
                    state = next;
                }
                if state.shutdown {
                    return;
                }
            }
            // ---- Hold the frame back until the throttle window elapses ----
            loop {
                let remaining = {
                    let Ok(state) = self.scheduler.state.lock() else {
                        return;
                    };
                    match interval.checked_sub(state.last_render_at.elapsed()) {
                        Some(remaining) => remaining,
                        None => break,
                    }
                };
                let Ok(state) = self.scheduler.state.lock() else {
                    return;
                };
                if state.shutdown {
                    return;
                }
                let Ok((state, _)) = self.scheduler.cv.wait_timeout(state, remaining) else {
                    return;
                };
                if state.shutdown {
                    return;
                }
            }
            self.flush_pending_render();
        }
    }

    /// Paint a frame parked by [`TUI::request_render`] /
    /// [`TuiAltScreen::request_render_reusing_scroll_content`].
    ///
    /// Returns `true` when a frame was painted, `false` when nothing was owed
    /// (or the owed frame was dropped because the TUI is stopped or a foreign
    /// runtime owns the terminal — both of those paths force a fresh full frame
    /// when they end, so nothing is lost on screen).
    ///
    /// The scheduler thread calls this; it is also the deterministic hook a host
    /// tick or a unit test can use to flush a request synchronously.
    pub fn flush_pending_render(&self) -> bool {
        let Some(reuse_scroll_content) = self.scheduler.take_pending() else {
            return false;
        };
        if !self.is_running() || self.is_render_suspended() {
            return false;
        }
        self.do_render(reuse_scroll_content);
        true
    }

    /// Park a frame request, or paint it inline when no scheduler is running.
    fn park_render(&self, reuse_scroll_content: bool) {
        if !self.scheduler_started.load(Ordering::Acquire) {
            self.do_render(reuse_scroll_content);
            return;
        }
        self.scheduler.park(reuse_scroll_content);
    }

    /// Test seam: enable frame parking *without* spawning the scheduler thread,
    /// so a test can drive [`TuiAltScreen::flush_pending_render`] itself and
    /// observe the coalescing deterministically.
    #[cfg(test)]
    fn enable_render_coalescing(&self) {
        self.scheduler_started.store(true, Ordering::Release);
    }

    /// Record the frame timestamp that gates the next coalesced repaint.
    fn mark_frame_painted(&self) {
        self.scheduler.mark_painted();
    }

    /// Use the terminal's main screen and native scrollback instead of the
    /// constrained alternate-screen viewport. Must be set before start.
    pub fn set_main_screen_mode(&self, enabled: bool) {
        if !self.is_running() {
            *self.main_screen_mode.lock().unwrap() = enabled;
        }
    }

    fn uses_main_screen(&self) -> bool {
        self.main_screen_mode
            .lock()
            .map(|mode| *mode)
            .unwrap_or(false)
    }

    /// Set the layout root component.
    pub fn set_layout_root(&self, component: Option<Arc<dyn Component>>) {
        if let Ok(mut root) = self.layout_root.lock() {
            *root = component;
        }
        self.request_render(false);
    }

    /// Get the layout root.
    pub fn get_layout_root(&self) -> Option<Arc<dyn Component>> {
        self.layout_root.lock().ok()?.clone()
    }

    fn is_running(&self) -> bool {
        self.running.lock().map(|running| *running).unwrap_or(false)
    }

    /// Temporarily suspend the outer application renderer while a foreign
    /// runtime owns the terminal (for example a Node extension's fullscreen
    /// component). The terminal remains in raw/alternate-screen mode; only
    /// background repaint requests are suppressed.
    pub fn set_render_suspended(&self, suspended: bool) {
        if let Ok(mut value) = self.render_suspended.lock() {
            *value = suspended;
        }
        if !suspended {
            self.request_render(true);
        }
    }

    pub fn is_render_suspended(&self) -> bool {
        self.render_suspended
            .lock()
            .map(|value| *value)
            .unwrap_or(false)
    }

    /// Get the current scroll position.
    pub fn viewport_top(&self) -> usize {
        if let Some(scroll) = self.get_primary_scroll_view() {
            return scroll.scroll_top();
        }
        self.scroll_top.lock().map(|s| *s).unwrap_or(0)
    }

    /// Check if following output.
    pub fn is_following_output(&self) -> bool {
        if let Some(scroll) = self.get_primary_scroll_view() {
            return scroll.is_following_end();
        }
        self.stick_to_bottom.lock().map(|s| *s).unwrap_or(true)
    }

    /// Set a clickable label shown while the follow-end primary view is scrolled up.
    /// The label is recomputed for each frame, allowing live theme and key hints.
    pub fn set_scroll_to_end_indicator(
        &self,
        label: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    ) {
        self.jump_to_end.set_label(label);
        if self.is_running() {
            self.request_render_reusing_scroll_content();
        }
    }

    /// Handle a click before the host starts text selection or other mouse actions.
    pub fn handle_scroll_to_end_indicator_mouse_event(
        &self,
        event: &crossterm::event::MouseEvent,
    ) -> bool {
        if self.uses_main_screen()
            || self.is_render_suspended()
            || self.has_overlay()
            || !self.jump_to_end.hit(event)
        {
            return false;
        }
        let Some(scroll) = self.get_primary_scroll_view() else {
            return false;
        };
        if scroll.is_following_end() || !scroll.follows_end() {
            return false;
        }
        self.scroll_to_bottom();
        true
    }

    /// Feed a left-button mouse event to the fullscreen selection tracker.
    ///
    /// Must be called before the host interprets the event itself (the host
    /// checks overlays/indicator first, then this, then wheel scrolling).
    /// Returns true when the selection changed and a re-render was requested.
    pub fn handle_selection_mouse_event(&self, event: &crossterm::event::MouseEvent) -> bool {
        if self.uses_main_screen() || self.has_overlay() {
            return false;
        }
        let Some(frame) = self.current_frame.lock().ok().and_then(|f| f.clone()) else {
            return false;
        };
        // Resolve against the geometry actually painted, not the terminal's
        // live size. Taking the terminal lock here would also stall the input
        // reader behind render diffing and slow writes, queuing stale drags.
        let changed = self
            .selection
            .handle_mouse_event(event, &frame, frame.height, frame.width);
        if changed {
            self.request_render_reusing_scroll_content();
        }
        changed
    }

    /// The selected text, extracted from the scroll view's cached content
    /// rows (or the composited screen for selections outside any scroll
    /// view). None when no real drag has produced a non-empty selection.
    pub fn selection_text(&self) -> Option<String> {
        let frame = self.current_frame.lock().ok().and_then(|f| f.clone())?;
        if !self.selection.dragged() {
            return None;
        }
        crate::selection::extract_selection_text(&self.selection, &frame)
    }

    /// Whether the last completed selection press was a real drag.
    pub fn selection_dragged(&self) -> bool {
        self.selection.dragged()
    }

    /// Drop any selection (e.g. when an overlay opens or the host takes the
    /// mouse for something else).
    pub fn clear_selection(&self) {
        self.selection.clear();
    }

    /// Scroll by the given number of lines.
    pub fn scroll_by(&self, lines: i32) {
        if let Some(scroll) = self.get_primary_scroll_view() {
            scroll.scroll_by(lines);
            self.request_render_reusing_scroll_content();
            return;
        }
        if let Ok(mut scroll_top) = self.scroll_top.lock() {
            if lines > 0 {
                *scroll_top = scroll_top.saturating_add(lines as usize);
            } else {
                *scroll_top = scroll_top.saturating_sub((-lines) as usize);
            }
        }
        if lines != 0 {
            if let Ok(mut stick) = self.stick_to_bottom.lock() {
                *stick = lines > 0; // Re-enable stick on scroll down
            }
        }
        self.request_render(false);
    }

    /// Scroll to the top.
    pub fn scroll_to_top(&self) {
        if let Some(scroll) = self.get_primary_scroll_view() {
            scroll.scroll_to_start();
            self.request_render_reusing_scroll_content();
            return;
        }
        if let Ok(mut scroll_top) = self.scroll_top.lock() {
            *scroll_top = 0;
        }
        if let Ok(mut stick) = self.stick_to_bottom.lock() {
            *stick = false;
        }
        self.request_render(false);
    }

    /// Scroll to the bottom.
    pub fn scroll_to_bottom(&self) {
        if let Some(scroll) = self.get_primary_scroll_view() {
            scroll.scroll_to_end();
            self.request_render_reusing_scroll_content();
            return;
        }
        if let Ok(mut stick) = self.stick_to_bottom.lock() {
            *stick = true;
        }
        self.request_render(false);
    }

    /// Get the primary scroll view if any.
    pub fn get_primary_scroll_view(&self) -> Option<Arc<ScrollView>> {
        self.current_frame
            .lock()
            .ok()?
            .as_ref()?
            .primary_scroll_view
            .clone()
    }

    /// Get the current terminal column count (cached, refreshed on resize).
    /// Used by callers that need a width for off-layout rendering (e.g.
    /// diff-line width sizing) without re-reading the terminal themselves.
    pub fn width(&self) -> usize {
        self.terminal.lock().map(|t| t.columns()).unwrap_or(80)
    }

    /// Set the terminal window/tab title.
    ///
    /// The [`Terminal`] trait's `set_title` on the real `ProcessTerminal` emits
    /// `\x1b]2;{title}\x07` (OSC 2); the `DummyTerminal` stub is a no-op. This
    /// accessor locks the real underlying terminal (bypassing the no-op trait
    /// impl returned by [`TUI::terminal`]) so hosts can reflect run state in
    /// the window title — e.g. "rpi — working" while a turn is in flight.
    pub fn set_title(&self, title: &str) {
        if let Ok(terminal) = self.terminal.lock() {
            terminal.set_title(title);
            terminal.flush();
        }
    }

    /// Check if this implements ViewportTUI.
    pub fn is_viewport_tui(&self) -> bool {
        true
    }

    /// Re-read the cached terminal size and force a full alternate-screen repaint.
    ///
    /// Kept for existing hosts that call this from their resize-event handler.
    /// Even an unchanged size can require repainting after terminal reflow;
    /// use [`TuiAltScreen::ensure_size`] for periodic size checks instead.
    pub fn refresh_size(&self) {
        if let Ok(terminal) = self.terminal.lock() {
            terminal.refresh_size();
        }
        self.note_resize();
    }

    /// A resize event reached the loop: schedule one full-screen repaint.
    ///
    /// The event's dimensions are not used (on Windows they carry the console
    /// buffer size, not the window) — the renderer re-reads the size itself.
    /// The repaint is forced even when the window ends up at the size it already
    /// had, because every intermediate resize reflowed the terminal's rows.
    pub fn note_resize(&self) {
        // A resize can change the cell size in pixels (font zoom, moving to a
        // monitor with a different DPI). Image sizing divides the source image
        // by the cell size, so a stale cache would keep the old geometry. This
        // is a no-op on terminals that do not report pixel sizes.
        self.poll_cell_dimensions();
        self.resize_dirty.store(true, Ordering::Release);
        self.request_render(false);
    }

    /// Watchdog: re-read the terminal size and repaint when it differs from the
    /// geometry currently on screen.
    ///
    /// A resize *event* is only a wake-up — on Windows its record carries the
    /// console buffer size and can arrive before the window rectangle moves —
    /// so hosts call this from their periodic tick. The render path re-reads the
    /// size on every frame too; this catches a window that changed while no
    /// event reached the loop.
    ///
    /// The cell size in pixels is polled here as well, because the character
    /// grid can stay identical while the cell geometry changes (font zoom, a
    /// move to a monitor with another DPI) and no resize event is guaranteed to
    /// arrive for that. A geometry change repaints every row: image placements
    /// re-derive their row count from the cell size, so a diff against the old
    /// frame would keep rows at the previous geometry.
    pub fn ensure_size(&self) {
        let Some((columns, rows)) = self.terminal.lock().ok().map(|terminal| {
            terminal.refresh_size();
            (terminal.columns(), terminal.rows())
        }) else {
            return;
        };
        if columns == 0 || rows == 0 {
            return;
        }
        let size_changed = *self.painted_size.lock().unwrap() != (columns, rows);
        self.poll_cell_dimensions();
        let cells_changed = self.cell_geometry_changed();
        if !size_changed && !cells_changed {
            return;
        }
        // Alt-screen frames need every row repainted: the terminal reflowed the
        // old frame, so it can no longer be patched cell by cell. Main-screen
        // mode keeps its document and rebuilds it when it sees the size change.
        if !self.uses_main_screen() {
            if let Ok(mut prev) = self.previous_screen.lock() {
                prev.clear();
            }
        }
        self.resize_dirty.store(true, Ordering::Release);
        self.request_render(false);
    }

    /// The cell geometry this renderer has polled, polling once if it has not.
    ///
    /// Reading through this instead of [`crate::terminal_image::get_cell_dimensions`]
    /// keeps the renderer's own view of the geometry separate from the
    /// process-wide cache, which other terminals (and, in tests, other
    /// renderers) also write.
    fn current_cell_dimensions(&self) -> CellDimensions {
        if let Some(cells) = *self.cell_geometry.lock().unwrap() {
            return cells;
        }
        self.poll_cell_dimensions()
    }

    /// Poll the terminal's pixel size and record it on this renderer.
    ///
    /// A terminal that cannot report a pixel size leaves the previously polled
    /// geometry in place, so a transient `Unsupported` reply cannot reset an
    /// established geometry mid-session. Before the first successful poll the
    /// process-wide cache supplies the fallback, which keeps a terminal that
    /// never reports pixels rendering at the historical default.
    fn poll_cell_dimensions(&self) -> CellDimensions {
        let polled = self
            .terminal
            .lock()
            .ok()
            .and_then(|terminal| poll_cell_dimensions_of(terminal.as_ref()));
        self.record_cell_dimensions(polled)
    }

    /// Poll through a terminal lock the caller already holds.
    ///
    /// Renderers take the terminal lock for the whole frame; re-locking here
    /// would deadlock against themselves.
    fn poll_cell_dimensions_with(&self, terminal: &dyn Terminal) -> CellDimensions {
        self.record_cell_dimensions(poll_cell_dimensions_of(terminal))
    }

    fn record_cell_dimensions(&self, polled: Option<CellDimensions>) -> CellDimensions {
        let mut slot = self.cell_geometry.lock().unwrap();
        if let Some(polled) = polled {
            *slot = Some(polled);
        }
        slot.unwrap_or_else(crate::terminal_image::get_cell_dimensions)
    }

    /// Whether the polled cell geometry differs from the frame on screen.
    ///
    /// Compared against `painted_cells` (what the last frame was laid out for),
    /// not against the previous poll: a geometry change that arrived while a
    /// repaint was parked must still be picked up by the next tick.
    fn cell_geometry_changed(&self) -> bool {
        let current = self.current_cell_dimensions();
        *self.painted_cells.lock().unwrap() != current
    }

    fn enter_alt_screen(&self) {
        if let Ok(terminal) = self.terminal.lock() {
            // Enter alternate screen buffer and disable autowrap
            terminal.write("\x1b[?1049h\x1b[?7l");
            // Clear screen and hide cursor
            terminal.write("\x1b[2J\x1b[H\x1b[?25l");
            // A fresh alternate buffer holds no images: the next frame has to
            // carry each payload again instead of placing it by id.
            self.invalidate_kitty_uploads();
            terminal.flush();
        }
    }

    /// Exit alternate screen mode.
    fn exit_alt_screen(&self, preserve_screen: bool) {
        if let Ok(terminal) = self.terminal.lock() {
            // Autowrap is a terminal mode, not restored by leaving the buffer.
            terminal.write("\x1b[?7h");
            if preserve_screen {
                terminal.write("\x1b[?1049l\x1b[?25h");
            } else {
                // Exit the alt buffer CLEANLY: back to the main buffer, clear
                // it, and show the cursor. The old path re-rendered the full
                // TUI frame (borders, backgrounds, spinner state, cursor
                // markers) into the main buffer, which is exactly the garbage
                // that made quitting look scrambled. The caller prints its own
                // farewell after this.
                terminal.write("\x1b[?1049l\x1b[2J\x1b[H\x1b[?25h");
            }
            // Whatever the terminal kept for the alternate buffer is gone with
            // it; the next frame has to carry the payloads again.
            self.invalidate_kitty_uploads();
            terminal.flush();
        }
    }

    /// Forget the image payloads this screen believes its terminal holds.
    ///
    /// Called for every event that can drop the terminal's image store: a
    /// screen clear, a resize, and entering or leaving the alternate screen.
    fn invalidate_kitty_uploads(&self) {
        self.kitty_uploads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .invalidate();
    }

    /// Hand the terminal back to the shell so the process can be stopped
    /// (upstream's `handleCtrlZ`): pause the renderer, leave the alternate
    /// buffer, and restore cooked mode. [`TuiAltScreen::resume`] reverses it
    /// once `SIGTSTP` returns.
    ///
    /// Unlike the private `stop`, this keeps the render scheduler alive, so the
    /// same TUI can be resumed in place.
    pub fn suspend(&self) {
        self.set_render_suspended(true);
        if let Ok(terminal) = self.terminal.lock() {
            terminal.disable_mouse();
            terminal.exit_raw_mode();
            terminal.flush();
        }
        if !self.uses_main_screen() {
            // `preserve_screen`: leave the alt buffer without clearing the
            // user's main-buffer scrollback (unlike a normal shutdown).
            self.exit_alt_screen(true);
        }
    }

    /// Re-enter raw mode and the alternate buffer after the process resumes
    /// from `SIGTSTP`, then force a full repaint (the alt buffer came back
    /// empty — `set_render_suspended` is what schedules it).
    pub fn resume(&self) {
        if !self.uses_main_screen() {
            self.enter_alt_screen();
        }
        if let Ok(terminal) = self.terminal.lock() {
            terminal.enter_raw_mode();
            if self.uses_main_screen() {
                terminal.disable_mouse();
            } else {
                terminal.enable_mouse();
            }
            terminal.flush();
        }
        self.set_render_suspended(false);
    }

    /// Render the complete component tree into the main terminal buffer. This
    /// follows upstream's regular-mode strategy: append growth with CRLF so
    /// the terminal creates real scrollback, and rewrite only the changed tail.
    fn do_render_main_screen(&self) {
        let Ok(_render_guard) = self.render_lock.lock() else {
            return;
        };
        // A render request may have passed the public suspension check while
        // a custom UI was opening. Re-check after taking the render lock so an
        // already queued outer frame cannot overwrite the foreign screen.
        if self.is_render_suspended() {
            return;
        }
        let Ok(terminal) = self.terminal.lock() else {
            return;
        };
        // Same rule as the alt-screen path: lay the document out for the size
        // the window has *now*, not for the last size an event reported.
        terminal.refresh_size();
        let width = terminal.columns();
        let height = terminal.rows();
        // The main screen is the other half of the same invariant: an image's
        // row count is derived from the cell geometry, so a document laid out
        // for a stale geometry is wrong even when the columns did not move.
        // Polling here too means a host that renders without a periodic tick
        // still picks up a font zoom / DPI change instead of only noticing it
        // through `ensure_size`. The terminal lock is already held, so this
        // polls through it rather than re-locking.
        let cells = self.poll_cell_dimensions_with(terminal.as_ref());
        let root: Arc<dyn Component> = self
            .get_layout_root()
            .unwrap_or_else(|| Arc::new(self.container.clone()));
        // Components inside the frame read the geometry this renderer polled
        // (frame-scoped), not the process-wide cache.
        let raw_lines = crate::render::with_main_screen_frame(|| {
            crate::terminal_image::with_frame_cell_geometry(cells, || root.render(width))
        });
        let cursor = raw_lines.iter().enumerate().rev().find_map(|(row, line)| {
            line.find(CURSOR_MARKER)
                .map(|idx| (row, visible_width(&line[..idx])))
        });
        let lines: Vec<String> = raw_lines
            .into_iter()
            .map(|line| line.replace(CURSOR_MARKER, ""))
            .collect();
        let previous = self
            .previous_screen
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();
        // The previous frame's geometry, recorded by whichever renderer ran
        // last. A mismatch means the terminal reflowed its rows.
        let (previous_width, previous_height) = *self.painted_size.lock().unwrap();
        let previous_cells = *self.painted_cells.lock().unwrap();
        let previous_cursor = *self.main_previous_cursor.lock().unwrap();
        terminal.write(&crate::terminal_image::removed_kitty_placements(
            &previous, &lines,
        ));

        let mut first_changed = None;
        let mut last_changed = None;
        for index in 0..previous.len().max(lines.len()) {
            if previous.get(index) != lines.get(index) {
                first_changed.get_or_insert(index);
                last_changed = Some(index);
            }
        }
        let viewport_top = *self.main_viewport_top.lock().unwrap();
        let force_repaint = self.main_force_repaint.swap(false, Ordering::AcqRel);
        if force_repaint && !previous.is_empty() {
            let first = viewport_top.min(lines.len().saturating_sub(1));
            first_changed = Some(first_changed.map_or(first, |changed| changed.min(first)));
            let last = lines.len().saturating_sub(1);
            last_changed = Some(last_changed.map_or(last, |changed| changed.max(last)));
        }
        let changed_in_scrollback = first_changed.is_some_and(|first| {
            first < viewport_top || lines.len().saturating_sub(1) < viewport_top
        });

        // Saved scrollback cannot be addressed by cursor movement or CSI T:
        // SD only inserts blanks in the live screen. Rebuild the document when
        // an old row changes, otherwise repainting its tail appends duplicate
        // history until a bounded terminal buffer evicts the conversation start.
        // Size changes also require a rebuild; ordinary screen-local updates
        // remain incremental.
        if (previous_width != 0 && previous_width != width)
            || (previous_height != 0 && previous_height != height)
            || previous_cells != cells
            || changed_in_scrollback
        {
            terminal.write("\x1b[?2026h");
            terminal.write("\x1b[2J\x1b[H\x1b[3J");
            self.invalidate_kitty_uploads();
            terminal.write(&lines.join("\r\n"));
            terminal.write("\x1b[?2026l");
            *self.main_hardware_row.lock().unwrap() = lines.len().saturating_sub(1);
            *self.main_viewport_top.lock().unwrap() = lines.len().saturating_sub(height);
        } else if previous.is_empty() {
            terminal.write("\x1b[?2026h");
            terminal.write(&lines.join("\r\n"));
            terminal.write("\x1b[?2026l");
            *self.main_hardware_row.lock().unwrap() = lines.len().saturating_sub(1);
            *self.main_viewport_top.lock().unwrap() = lines.len().saturating_sub(height);
        } else {
            if first_changed.is_none() && cursor == previous_cursor {
                return;
            }
            if let (Some(first), Some(last)) = (first_changed, last_changed) {
                let mut hardware_row = *self.main_hardware_row.lock().unwrap();
                let mut viewport_top = *self.main_viewport_top.lock().unwrap();
                let viewport_bottom = viewport_top + height.saturating_sub(1);
                let deleted_tail_only = first >= lines.len();
                let append_start = first == previous.len() && first > 0;
                let move_target = if deleted_tail_only {
                    lines.len().saturating_sub(1)
                } else if append_start {
                    first - 1
                } else {
                    first
                };
                let mut output = String::from("\x1b[?2026h");
                if move_target > viewport_bottom {
                    let current_screen = hardware_row
                        .saturating_sub(viewport_top)
                        .min(height.saturating_sub(1));
                    let down = height.saturating_sub(1).saturating_sub(current_screen);
                    if down > 0 {
                        output.push_str(&format!("\x1b[{down}B"));
                    }
                    let scroll = move_target - viewport_bottom;
                    output.push_str(&"\r\n".repeat(scroll));
                    viewport_top += scroll;
                    hardware_row = move_target;
                }
                let current_screen = hardware_row.saturating_sub(viewport_top);
                let target_screen = move_target.saturating_sub(viewport_top);
                if target_screen > current_screen {
                    output.push_str(&format!("\x1b[{}B", target_screen - current_screen));
                } else if current_screen > target_screen {
                    output.push_str(&format!("\x1b[{}A", current_screen - target_screen));
                }
                output.push_str(if append_start { "\r\n" } else { "\r" });
                let render_end = last.min(lines.len().saturating_sub(1));
                if !deleted_tail_only {
                    for index in first..=render_end {
                        if index > first {
                            output.push_str("\r\n");
                        }
                        if force_repaint {
                            // Repair unknown cells beyond the logical line too.
                            // Clear before writing: ED/EL after a full-width row
                            // can erase its last cell on pending-wrap terminals.
                            output.push_str("\x1b[2K");
                        }
                        if let Some((column, text)) = previous
                            .get(index)
                            .filter(|_| !force_repaint)
                            .and_then(|old| crate::line_diff::patch(old, &lines[index]))
                        {
                            if column > 0 {
                                output.push_str(&format!("\x1b[{column}C"));
                            }
                            output.push_str(&text);
                        } else {
                            output.push_str(&lines[index]);
                            if index < previous.len()
                                && visible_width(&lines[index]) < visible_width(&previous[index])
                            {
                                output.push_str("\x1b[K");
                            }
                        }
                    }
                }
                let mut final_row = render_end;
                if previous.len() > lines.len() {
                    let extra_lines = previous.len() - lines.len();
                    let clear_start_offset = usize::from(!lines.is_empty());
                    if clear_start_offset > 0 {
                        output.push_str("\x1b[1B");
                    }
                    for index in 0..extra_lines {
                        output.push_str("\r\x1b[2K");
                        if index + 1 < extra_lines {
                            output.push_str("\x1b[1B");
                        }
                    }
                    let move_back = extra_lines.saturating_sub(1) + clear_start_offset;
                    if move_back > 0 {
                        output.push_str(&format!("\x1b[{move_back}A"));
                    }
                    if extra_lines > 0 {
                        final_row = lines.len().saturating_sub(1);
                    }
                }
                if force_repaint {
                    // Short documents also own blank live rows below their
                    // tail. Clear these without scrolling or erasing saved rows,
                    // then return to the row recorded in the physical ledger.
                    let unused = height.saturating_sub(final_row.saturating_sub(viewport_top) + 1);
                    for _ in 0..unused {
                        output.push_str("\x1b[1B\r\x1b[2K");
                    }
                    if unused > 0 {
                        output.push_str(&format!("\x1b[{unused}A"));
                    }
                }
                output.push_str("\x1b[?2026l");
                terminal.write(&output);
                let advanced = final_row.saturating_sub(viewport_top + height.saturating_sub(1));
                viewport_top += advanced;
                *self.main_hardware_row.lock().unwrap() = final_row;
                *self.main_viewport_top.lock().unwrap() = viewport_top;
            }
        }

        if let Some((row, col)) = cursor {
            let hardware_row = *self.main_hardware_row.lock().unwrap();
            let mut viewport_top = *self.main_viewport_top.lock().unwrap();
            let viewport_bottom = viewport_top + height.saturating_sub(1);

            if row > viewport_bottom {
                // The editor row is below the visible window (a write scrolled
                // up to an edit above the old viewport). Scroll back down so
                // the editor is the bottom row, then move the cursor there.
                let scroll = row - viewport_bottom;
                let mut output = String::new();
                let current_screen = hardware_row
                    .saturating_sub(viewport_top)
                    .min(height.saturating_sub(1));
                let down = height.saturating_sub(1).saturating_sub(current_screen);
                if down > 0 {
                    output.push_str(&format!("\x1b[{down}B"));
                }
                output.push_str(&"\r\n".repeat(scroll));
                terminal.write(&output);
                viewport_top += scroll;
            } else if row < viewport_top {
                // Saved rows are not addressable: CSI T inserts blanks in the
                // live screen, it never retrieves scrollback. Keep the real
                // cursor/viewport ledger and hide the off-screen logical cursor.
                // It will be restored when a later layout puts it on screen.
                terminal.write("\x1b[?25l");
            } else {
                // Within the window: a relative move is enough (and never
                // scrolls, so it cannot drift the viewport bookkeeping).
                let current_screen = hardware_row
                    .saturating_sub(viewport_top)
                    .min(height.saturating_sub(1));
                let target_screen = row - viewport_top;
                if target_screen > current_screen {
                    terminal.write(&format!("\x1b[{}B", target_screen - current_screen));
                } else if current_screen > target_screen {
                    terminal.write(&format!("\x1b[{}A", current_screen - target_screen));
                }
            }

            if row >= viewport_top {
                terminal.write("\r");
                if col > 0 {
                    terminal.write(&format!("\x1b[{col}C"));
                }
                if self.get_show_hardware_cursor() {
                    terminal.write("\x1b[?25h");
                } else {
                    terminal.write("\x1b[?25l");
                }
                *self.main_hardware_row.lock().unwrap() = row;
                *self.main_viewport_top.lock().unwrap() = viewport_top;
            }
        } else {
            terminal.write("\x1b[?25l");
        }
        terminal.flush();
        *self.previous_screen.lock().unwrap() = lines;
        *self.main_previous_cursor.lock().unwrap() = cursor;
        if let Ok(mut painted) = self.painted_size.lock() {
            *painted = (width, height);
        }
        *self.painted_cells.lock().unwrap() = cells;
    }

    /// Perform a differential render with constrained layout.
    fn do_render(&self, reuse_scroll_content: bool) {
        if self.is_render_suspended() {
            return;
        }
        // upstream stamps the frame time immediately before `doRender()`; the
        // scheduler reads it to hold the next request back for the remainder of
        // the throttle window.
        self.mark_frame_painted();
        if self.uses_main_screen() {
            self.do_render_main_screen();
            return;
        }
        let Ok(_render_guard) = self.render_lock.lock() else {
            return;
        };
        if self.is_render_suspended() {
            return;
        }

        let (width, height) = if let Ok(terminal) = self.terminal.lock() {
            // Re-read instead of trusting the last event/cached value: a
            // fullscreen frame must be laid out for the size the window has
            // *now*, otherwise a resize leaves the dock and borders drawn for
            // the previous geometry.
            terminal.refresh_size();
            (terminal.columns(), terminal.rows())
        } else {
            return;
        };

        // Re-poll the cell geometry before laying the frame out, for the same
        // reason as the character size above: an image's row count is derived
        // from it, so a frame laid out for a stale cell size is wrong even when
        // the columns did not move. Doing it here (not only in `ensure_size`)
        // covers a host that renders without a periodic tick.
        let cells = self.poll_cell_dimensions();

        // Get layout root or use container
        let root = self.get_layout_root();
        let root_component: Arc<dyn Component> =
            root.unwrap_or_else(|| Arc::new(self.container.clone()));

        // Reflow changes document row/column identities. Never reinterpret an
        // old selection against rows wrapped for a different terminal width.
        // Cancel before rendering so a non-dragged click can restore follow.
        if self.current_frame.lock().ok().is_some_and(|current| {
            current
                .as_ref()
                .is_some_and(|previous| previous.width != width)
        }) {
            self.selection.clear();
        }

        // Render layout frame. Components inside the frame read the geometry
        // this renderer polled (frame-scoped), not the process-wide cache.
        // EVERY component that can size an image renders inside this one
        // scope, overlays included: the body frame and the overlays of the
        // same frame must be laid out for the same cell geometry, or an
        // overlay image keeps the old row count while the body below it has
        // already resized.
        let (frame, mut visible) = crate::terminal_image::with_frame_cell_geometry(cells, || {
            let frame = if reuse_scroll_content {
                render_layout_frame_reusing_scroll_content(root_component.clone(), width, height)
            } else {
                render_layout_frame(root_component.clone(), width, height)
            };
            // Get screen lines and composite modal overlays above the layout.
            let mut visible = frame.lines.clone();
            self.jump_to_end
                .paint(&frame, &mut visible, self.has_overlay());
            self.paint_overlays(&mut visible, width, height);
            (frame, visible)
        });

        // Cursor markers are an internal layout protocol, not terminal output.
        // Extract the location first, then strip every marker before diffing or
        // writing. Sending the APC marker to some Windows terminals caused the
        // character under the cursor to be erased when moving left/right.
        let cursor = extract_cursor_position(&visible, height);
        for line in &mut visible {
            *line = line.replace(CURSOR_MARKER, "");
        }

        // Reverse-video the selected span last, exactly where upstream applies
        // `applySelection`: after overlays so selection stays visible behind a
        // dialog, before the diff so the highlight reaches the terminal.
        crate::selection::apply_selection_highlight(&mut visible, &self.selection, &frame);

        // Check for full redraw
        let previous = self
            .previous_screen
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();
        let painted = *self.painted_size.lock().unwrap();
        let resize_dirty = self.resize_dirty.swap(false, Ordering::AcqRel);
        // A resize reflows the terminal's rows, so a frame captured at another
        // geometry (or before a resize notification) can no longer be patched
        // cell-by-cell: the skipped "unchanged" rows would keep the terminal's
        // reflowed leftovers (short borders, duplicated spinner rows). Repaint
        // every row when it moved.
        //
        // The cell size counts as geometry too: an image's row count is derived
        // from it, so a frame laid out for another cell size holds placements of
        // the wrong height even though every column count matches.
        let cells_changed = *self.painted_cells.lock().unwrap() != cells;
        let full_redraw =
            previous.is_empty() || painted != (width, height) || resize_dirty || cells_changed;

        // Build output buffer
        let mut buffer = String::new();
        buffer.push_str(&crate::terminal_image::removed_kitty_placements(
            &previous, &visible,
        ));

        if let Ok(terminal) = self.terminal.lock() {
            let mut uploads = self
                .kitty_uploads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if full_redraw {
                if let Ok(mut count) = self.full_redraw_count.lock() {
                    *count += 1;
                }
                // Clear, then re-assert "no autowrap": a full frame fills the
                // whole width, and a terminal that reset the mode on resize
                // would wrap the last cell and shift every following row.
                buffer.push_str("\x1b[2J");
                // A clear drops every image the terminal holds, so the next
                // image row has to carry its payload again.
                uploads.invalidate();
                buffer.push_str("\x1b[?7l");
            }

            // Begin synchronized output
            buffer.push_str("\x1b[?2026h");

            // Write changed lines
            for (row, line) in visible.iter().enumerate() {
                if !full_redraw && row < previous.len() && &previous[row] == line {
                    continue; // Skip unchanged lines
                }
                // A repainted image row carries the whole PNG payload. Once the
                // data is in the terminal, the placement alone is enough, which
                // keeps scrolling a transcript full of screenshots cheap.
                let is_image = crate::terminal_image::is_image_line(line);
                let placement = crate::terminal_image::kitty_placement_line(line, &uploads);
                let rendered = placement.as_deref().unwrap_or(line.as_str());
                // Image rows are never patched: their prefix is the escape that
                // changes, so a partial write buys nothing and a full row keeps
                // the placement anchored at column 1.
                if !is_image {
                    // Overwrite before clearing any leftover tail. Clearing the
                    // whole row first blanks stationary sidebars during scroll on
                    // terminals that do not implement synchronized output.
                    if let Some((column, text)) = (!full_redraw)
                        .then(|| crate::line_diff::patch(&previous[row], line))
                        .flatten()
                    {
                        buffer.push_str(&format!("\x1b[{};{}H{}", row + 1, column + 1, text));
                        continue;
                    }
                }
                buffer.push_str(&format!("\x1b[{};1H{}", row + 1, rendered));
                if !full_redraw && visible_width(line) < visible_width(&previous[row]) {
                    buffer.push_str("\x1b[K");
                }
            }

            // Every image on screen this frame is in the terminal: keep it
            // trusted for the next frame. An image that scrolled away is
            // forgotten, so its payload is re-sent when it comes back instead
            // of placing an id the terminal may no longer hold.
            for line in &visible {
                if let Some(id) = crate::terminal_image::kitty_transmission_id(line) {
                    uploads.note_painted(id);
                }
            }
            uploads.end_frame();

            // Position the hardware cursor using the marker location captured
            // before internal markers were stripped from `visible`.
            if let Some((row, col)) = cursor {
                buffer.push_str(&format!("\x1b[{};{}H", row + 1, col + 1));
                if self.get_show_hardware_cursor() {
                    buffer.push_str("\x1b[?25h");
                }
            } else {
                // Hide cursor if no cursor marker found
                buffer.push_str("\x1b[?25l");
            }

            // End synchronized output
            buffer.push_str("\x1b[?2026l");

            terminal.write(&buffer);
            terminal.flush();
        }

        // Publish input geometry only after the synchronized frame is written.
        // Its scroll snapshots must describe the screen the user actually saw,
        // not a frame still being assembled or an already-mutated live cache.
        if let Ok(mut current) = self.current_frame.lock() {
            *current = Some(Arc::new(frame));
        }
        // Store current screen
        if let Ok(mut prev) = self.previous_screen.lock() {
            *prev = visible;
        }
        if let Ok(mut painted) = self.painted_size.lock() {
            *painted = (width, height);
        }
        *self.painted_cells.lock().unwrap() = cells;
    }

    fn paint_overlays(&self, lines: &mut [String], width: usize, height: usize) {
        for overlay in self.overlays.render_visible(width, height) {
            let (x, y, overlay_width) = (overlay.x, overlay.y, overlay.width);
            let overlay_lines = overlay.lines;
            let overlay_height = overlay_lines.len();
            for (index, line) in overlay_lines.iter().take(overlay_height).enumerate() {
                let row = y + index;
                if row >= lines.len() {
                    break;
                }
                lines[row] = composite_tui_line(&lines[row], line, x, overlay_width, width);
            }
        }
    }
}

impl Component for TuiAltScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.get_layout_root()
            .map(|r| r.render(width))
            .unwrap_or_else(|| self.container.render(width))
    }

    fn invalidate(&self) {
        if let Some(root) = self.get_layout_root() {
            root.invalidate();
        } else {
            self.container.invalidate();
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl TUI for TuiAltScreen {
    fn mode(&self) -> TuiMode {
        if self.uses_main_screen() {
            TuiMode::Regular
        } else {
            TuiMode::Fullscreen
        }
    }

    fn terminal(&self) -> &dyn Terminal {
        &self.terminal_proxy
    }

    fn children(&self) -> Vec<Arc<dyn Component>> {
        self.container.get_children()
    }

    fn add_child(&self, component: Arc<dyn Component>) {
        self.container.add_child(component);
    }

    fn remove_child(&self, component: &Arc<dyn Component>) {
        self.container.remove_child(component);
    }

    fn clear(&self) {
        self.container.clear();
    }

    fn get_show_hardware_cursor(&self) -> bool {
        self.show_hardware_cursor
            .lock()
            .map(|s| *s)
            .unwrap_or(false)
    }

    fn set_show_hardware_cursor(&self, enabled: bool) {
        if let Ok(mut show) = self.show_hardware_cursor.lock() {
            *show = enabled;
        }
    }

    fn get_clear_on_shrink(&self) -> bool {
        self.clear_on_shrink.lock().map(|s| *s).unwrap_or(false)
    }

    fn set_clear_on_shrink(&self, enabled: bool) {
        if let Ok(mut clear) = self.clear_on_shrink.lock() {
            *clear = enabled;
        }
    }

    fn set_focus(&self, component: Option<Arc<dyn Component>>) {
        if let Ok(mut focused) = self.focused.lock() {
            *focused = component;
        }
    }

    fn get_focus(&self) -> Option<Arc<dyn Component>> {
        self.focused.lock().ok()?.clone()
    }

    fn show_overlay(
        &self,
        component: Arc<dyn Component>,
        options: Option<OverlayOptions>,
    ) -> Arc<dyn OverlayHandle> {
        let handle = self.overlays.add(component, options.unwrap_or_default());
        self.request_render(false);
        Arc::new(ManagedOverlayHandle {
            handle,
            hidden: Mutex::new(false),
            focused: Mutex::new(true),
        })
    }

    fn hide_overlay(&self) {
        self.overlays.remove_topmost();
        self.request_render(false);
    }

    fn has_overlay(&self) -> bool {
        !self.overlays.get_visible().is_empty()
    }

    fn start(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }

        // Clone the necessary Arc references for the closures
        let running = self.running.clone();

        // Keyboard protocol stacks belong to each screen buffer. Switch first
        // so the terminal enables its protocol on the screen that receives input.
        if !self.uses_main_screen() {
            self.enter_alt_screen();
        }

        // Get terminal and start it (spawns its own input-reader thread whose
        // callbacks are stubs below; kept for the non-pi-tui callers that still
        // rely on `start()`). The asynchronous interactive path uses
        // `start_readerless` instead to avoid a competing stdin reader.
        if let Ok(terminal) = self.terminal.lock() {
            terminal.start(
                Box::new(move |event: InputEvent| {
                    if !*running.lock().unwrap() {
                        return;
                    }
                    let _ = event; // stub: input handled by the caller's own loop
                }),
                Box::new(move || {}),
            );
            if self.uses_main_screen() {
                terminal.disable_mouse();
            } else {
                terminal.enable_mouse();
            }
            terminal.flush();
        }

        self.do_render(false);
    }

    fn stop(&self, options: TuiStopOptions) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }

        // Stop the coalescing scheduler *before* the terminal goes away, and
        // join it so a frame already in flight cannot repaint the screen after
        // shutdown (the class of bug the 0.1.18 regular-mode fix targets).
        self.scheduler.shutdown();
        if let Some(handle) = self.scheduler_thread.lock().ok().and_then(|mut s| s.take()) {
            let _ = handle.join();
        }

        if self.uses_main_screen() {
            if let Ok(terminal) = self.terminal.lock() {
                // Leave the shell prompt below the rendered footer while
                // preserving all conversation rows in terminal scrollback.
                terminal.write("\x1b[?25h\r\n");
                terminal.stop();
            }
        } else {
            // Pop the keyboard protocol on its owning screen before leaving it.
            if let Ok(terminal) = self.terminal.lock() {
                terminal.stop();
            }
            self.exit_alt_screen(options.preserve_screen);
        }
    }

    fn render_now(&self, force: bool) {
        // Layout is assembled before `start()`. Rendering during that phase
        // writes the future TUI frame into the main screen, which is then
        // restored on exit and appears as uncleared output.
        if !self.is_running() {
            return;
        }
        if self.is_render_suspended() {
            return;
        }
        if force {
            if self.uses_main_screen() {
                // Forgetting the baseline while retaining a physical cursor
                // makes the "first frame" branch append the whole transcript at
                // that cursor. Preserve both ledgers and repaint live rows only.
                self.main_force_repaint.store(true, Ordering::Release);
            } else if let Ok(mut prev) = self.previous_screen.lock() {
                prev.clear();
            }
        }
        // An immediate frame supersedes a parked coalesced one: upstream's
        // `renderNow` clears `renderRequested` and cancels the render timer.
        self.scheduler.clear_pending();
        self.do_render(false);
    }

    fn request_render(&self, force: bool) {
        if self.is_render_suspended() {
            return;
        }
        if force {
            // upstream `requestRender(true)`: reset the diff state and paint an
            // immediate frame instead of waiting out the throttle window.
            self.render_now(true);
            return;
        }
        if !self.is_running() {
            return;
        }
        // Coalesced (upstream `requestRender(false)`): park the frame and let
        // the scheduler paint it at most once per `MIN_RENDER_INTERVAL_MS`.
        self.park_render(false);
    }

    fn full_redraws(&self) -> usize {
        self.full_redraw_count.lock().map(|c| *c).unwrap_or(0)
    }
}

impl TuiAltScreen {
    /// Set up the tty and render the first frame **without** spawning the
    /// `ProcessTerminal` input-reader thread. The caller owns the input loop
    /// (e.g. a `spawn_blocking` `event::read()` loop) and handles resize/key
    /// events directly via [`TuiAltScreen::note_resize`] / its key dispatch.
    ///
    /// The first frame is painted inline. Hosts that want the native-pi frame
    /// throttle should also call [`TuiAltScreen::start_render_scheduler`].
    ///
    /// This avoids two `event::read()` consumers racing the same stdin queue
    /// (the stub thread in [`TUI::start`] dropped a fraction of keystrokes).
    /// The caller is responsible for `enable_raw_mode`-dependent event reads,
    /// for calling `note_resize` on `Event::Resize`, and for running the
    /// `ensure_size` watchdog from a periodic tick.
    pub fn start_readerless(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }
        if !self.uses_main_screen() {
            self.enter_alt_screen();
        }
        if let Ok(terminal) = self.terminal.lock() {
            terminal.enter_raw_mode();
            if self.uses_main_screen() {
                // Give wheel/touchpad gestures back to the terminal emulator;
                // it can now scroll its native history smoothly.
                terminal.disable_mouse();
            } else {
                // On macOS enter_raw_mode intentionally leaves mouse tracking
                // disabled so the main screen can use native scrollback. The
                // alternate-screen transcript owns scrolling, however, so it
                // must explicitly re-enable wheel/touchpad events here.
                terminal.enable_mouse();
            }
            terminal.flush();
        }
        self.do_render(false);
    }

    /// Render a frame while reusing scroll-view content. This is appropriate
    /// when only the viewport or bottom dock changed. A missing cache or width
    /// change falls back to rendering fresh content automatically.
    ///
    /// Coalesced like [`TUI::request_render`]; a request that upgrades the parked
    /// frame to a full transcript render wins.
    pub fn request_render_reusing_scroll_content(&self) {
        if !self.is_running() || self.is_render_suspended() {
            return;
        }
        self.park_render(true);
    }
}

/// Check if a TUI implements ViewportTUI.
pub fn is_viewport_tui(_tui: &dyn TUI) -> bool {
    _tui.as_any().is::<TuiAltScreen>()
}

struct ManagedOverlayHandle {
    handle: super::overlay::OverlayHandle,
    hidden: Mutex<bool>,
    focused: Mutex<bool>,
}

impl OverlayHandle for ManagedOverlayHandle {
    fn hide(&self) {
        self.handle.hide();
        if let Ok(mut hidden) = self.hidden.lock() {
            *hidden = true;
        }
    }

    fn set_hidden(&self, hidden: bool) {
        self.handle.set_visible(!hidden);
        if let Ok(mut current) = self.hidden.lock() {
            *current = hidden;
        }
    }

    fn is_hidden(&self) -> bool {
        self.hidden.lock().map(|hidden| *hidden).unwrap_or(true)
    }

    fn focus(&self) {
        if let Ok(mut focused) = self.focused.lock() {
            *focused = true;
        }
    }

    fn is_focused(&self) -> bool {
        self.focused.lock().map(|focused| *focused).unwrap_or(false)
    }
}

/// Shared terminal proxy exposed through [`TUI::terminal`]. The TUI owns the
/// terminal behind a mutex because render and input paths can run concurrently,
/// while callers of the trait need a stable `&dyn Terminal` reference.
struct TerminalProxy {
    terminal: Arc<Mutex<Box<dyn Terminal>>>,
}

impl TerminalProxy {
    fn with<R>(&self, f: impl FnOnce(&dyn Terminal) -> R) -> Option<R> {
        self.terminal
            .lock()
            .ok()
            .map(|terminal| f(terminal.as_ref()))
    }
}

impl Terminal for TerminalProxy {
    fn info(&self) -> TerminalInfo {
        self.with(|terminal| terminal.info()).unwrap_or_default()
    }

    fn write(&self, data: &str) {
        let _ = self.with(|terminal| terminal.write(data));
    }

    fn hide_cursor(&self) {
        let _ = self.with(|terminal| terminal.hide_cursor());
    }

    fn show_cursor(&self) {
        let _ = self.with(|terminal| terminal.show_cursor());
    }

    fn move_cursor(&self, row: usize, col: usize) {
        let _ = self.with(|terminal| terminal.move_cursor(row, col));
    }

    fn clear_screen(&self) {
        let _ = self.with(|terminal| terminal.clear_screen());
    }

    fn set_title(&self, title: &str) {
        let _ = self.with(|terminal| terminal.set_title(title));
    }

    fn enable_mouse(&self) {
        let _ = self.with(|terminal| terminal.enable_mouse());
    }

    fn disable_mouse(&self) {
        let _ = self.with(|terminal| terminal.disable_mouse());
    }

    fn enter_raw_mode(&self) {
        let _ = self.with(|terminal| terminal.enter_raw_mode());
    }

    fn exit_raw_mode(&self) {
        let _ = self.with(|terminal| terminal.exit_raw_mode());
    }

    fn refresh_size(&self) {
        let _ = self.with(|terminal| terminal.refresh_size());
    }

    fn window_size(&self) -> Option<crossterm::terminal::WindowSize> {
        self.with(|terminal| terminal.window_size()).flatten()
    }

    fn start(
        &self,
        on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
        on_resize: Box<dyn Fn() + Send + Sync>,
    ) {
        let _ = self.with(|terminal| terminal.start(on_input, on_resize));
    }

    fn stop(&self) {
        let _ = self.with(|terminal| terminal.stop());
    }

    fn is_tty(&self) -> bool {
        self.with(|terminal| terminal.is_tty()).unwrap_or(false)
    }

    fn set_progress(&self, active: bool) {
        let _ = self.with(|terminal| terminal.set_progress(active));
    }

    fn flush(&self) {
        let _ = self.with(|terminal| terminal.flush());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vstack::StackEntryOptions;
    use crate::{Editor, Focusable, ScrollViewOptions, Text, VStack};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// A terminal whose size the test controls, so a resize can be replayed
    /// exactly as the host input loop sees it.
    struct ResizableTerminal {
        output: Arc<Mutex<String>>,
        size: Arc<Mutex<(usize, usize)>>,
        /// The pixel size this fake terminal reports for its current grid.
        /// `None` means "cannot report pixels", the Windows / zero-ioctl case.
        pixels: Arc<Mutex<Option<(u16, u16)>>>,
    }

    impl ResizableTerminal {
        fn new(columns: usize, rows: usize) -> Self {
            Self {
                output: Arc::new(Mutex::new(String::new())),
                size: Arc::new(Mutex::new((columns, rows))),
                pixels: Arc::new(Mutex::new(None)),
            }
        }

        fn output(&self) -> Arc<Mutex<String>> {
            self.output.clone()
        }

        fn size(&self) -> Arc<Mutex<(usize, usize)>> {
            self.size.clone()
        }

        /// The pixel size this terminal reports, settable per test.
        ///
        /// The geometry a renderer uses is a property of its terminal, so a
        /// test drives it through the fake terminal instead of a process-wide
        /// override: nothing another test does can move this renderer's cell
        /// size between two frames.
        fn pixels(&self) -> Arc<Mutex<Option<(u16, u16)>>> {
            self.pixels.clone()
        }
    }

    impl Terminal for ResizableTerminal {
        fn info(&self) -> TerminalInfo {
            let (columns, rows) = *self.size.lock().unwrap();
            TerminalInfo {
                columns,
                rows,
                ..Default::default()
            }
        }

        fn write(&self, data: &str) {
            self.output.lock().unwrap().push_str(data);
        }

        fn hide_cursor(&self) {}
        fn show_cursor(&self) {}
        fn move_cursor(&self, _row: usize, _col: usize) {}
        fn clear_screen(&self) {}
        fn set_title(&self, _title: &str) {}
        fn enable_mouse(&self) {}
        fn disable_mouse(&self) {}
        fn enter_raw_mode(&self) {}
        fn exit_raw_mode(&self) {}
        fn refresh_size(&self) {}
        fn window_size(&self) -> Option<crossterm::terminal::WindowSize> {
            let (columns, rows) = *self.size.lock().unwrap();
            let (width, height) = (*self.pixels.lock().unwrap())?;
            Some(crossterm::terminal::WindowSize {
                columns: columns as u16,
                rows: rows as u16,
                width,
                height,
            })
        }
        fn start(
            &self,
            _on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
            _on_resize: Box<dyn Fn() + Send + Sync>,
        ) {
        }
        fn stop(&self) {}
        fn is_tty(&self) -> bool {
            true
        }
        fn set_progress(&self, _active: bool) {}
        fn flush(&self) {}
    }

    #[test]
    fn resize_repaints_every_row_at_the_dimensions_the_window_reports() {
        let terminal = ResizableTerminal::new(40, 8);
        let output = terminal.output();
        let size = terminal.size();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        let editor = Arc::new(Editor::simple());
        editor.set_focused(true);
        tui.set_layout_root(Some(editor));
        tui.start_readerless();
        output.lock().unwrap().clear();

        // A corner drag grows both axes. The new frame must cover the whole new
        // viewport instead of staying at the previous, smaller geometry (the
        // reported "UI still only fills the old small area").
        *size.lock().unwrap() = (60, 12);
        tui.ensure_size();

        let frame = output.lock().unwrap().clone();
        assert!(
            frame.contains("\x1b[2J"),
            "a resize must repaint the whole screen, got: {frame:?}"
        );
        assert!(
            frame.contains("\x1b[12;1H"),
            "the last row of the new viewport must be painted, got: {frame:?}"
        );
        assert!(
            !frame.contains("\x1b[13;1H"),
            "no row beyond the new height may be addressed, got: {frame:?}"
        );
        assert_eq!(*tui.painted_size.lock().unwrap(), (60, 12));
    }

    #[test]
    fn a_resize_event_never_lays_the_frame_out_for_a_size_the_window_does_not_have() {
        let terminal = ResizableTerminal::new(40, 8);
        let output = terminal.output();
        let size = terminal.size();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        let editor = Arc::new(Editor::simple());
        editor.set_focused(true);
        tui.set_layout_root(Some(editor));
        tui.start_readerless();

        // The resize notification arrives while the window still reports the old
        // geometry — exactly the Windows ordering while a corner is dragged. The
        // frame must stay on the size the window really has; writing rows for a
        // not-yet-existing size is what scrolls the dock away and stacks
        // leftovers behind.
        output.lock().unwrap().clear();
        tui.ensure_size();
        assert!(
            output.lock().unwrap().is_empty(),
            "a window that still reports the old size must not repaint"
        );

        // The window catches up; the next watchdog tick re-fits the frame.
        output.lock().unwrap().clear();
        *size.lock().unwrap() = (60, 12);
        tui.ensure_size();
        let frame = output.lock().unwrap().clone();
        assert!(frame.contains("\x1b[12;1H"), "got: {frame:?}");
        assert_eq!(*tui.painted_size.lock().unwrap(), (60, 12));
    }

    #[test]
    fn selection_burst_does_not_wait_for_terminal_output() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use std::sync::mpsc;

        let terminal = ResizableTerminal::new(40, 8);
        let output = terminal.output();
        let tui = Arc::new(TuiAltScreen::new(Box::new(terminal), false, None));
        tui.set_layout_root(Some(Arc::new(ScrollView::simple(Arc::new(Text::new(
            "prefix TARGET suffix",
            0,
            0,
        ))))));
        tui.start_readerless();
        tui.enable_render_coalescing();
        let painted = tui.current_frame.lock().unwrap().clone().unwrap();
        output.lock().unwrap().clear();

        // Model the render thread holding the terminal during a slow write.
        // Input must keep consuming events and park only the latest repaint.
        let terminal_guard = tui.terminal.lock().unwrap();
        let input_tui = tui.clone();
        let (tx, rx) = mpsc::channel();
        let input = std::thread::spawn(move || {
            let mouse = |kind, column| MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::NONE,
            };
            input_tui
                .handle_selection_mouse_event(&mouse(MouseEventKind::Down(MouseButton::Left), 7));
            for i in 0..200 {
                input_tui.handle_selection_mouse_event(&mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    8 + i % 10,
                ));
            }
            input_tui
                .handle_selection_mouse_event(&mouse(MouseEventKind::Up(MouseButton::Left), 12));
            tx.send(input_tui.selection_text()).unwrap();
        });
        let selected = rx.recv_timeout(Duration::from_secs(2));
        // Release and join even on failure, so a regression cannot hang the suite.
        drop(terminal_guard);
        input.join().unwrap();
        assert_eq!(
            selected
                .expect("mouse input must not wait for the terminal lock")
                .as_deref(),
            Some("TARGET")
        );
        assert!(output.lock().unwrap().is_empty());
        assert!(Arc::ptr_eq(
            &painted,
            tui.current_frame.lock().unwrap().as_ref().unwrap()
        ));
        assert!(tui.flush_pending_render());
        assert!(output.lock().unwrap().contains("\x1b[7mTARGET"));
        assert!(!Arc::ptr_eq(
            &painted,
            tui.current_frame.lock().unwrap().as_ref().unwrap()
        ));
        assert_eq!(painted.lines[0], "prefix TARGET suffix");
        assert!(!tui.flush_pending_render());
    }

    #[test]
    fn selection_uses_painted_dimensions_until_resize_is_rendered() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let terminal = ResizableTerminal::new(40, 8);
        let size = terminal.size();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        tui.set_layout_root(Some(Arc::new(Text::new(
            "header\nmiddle\nprefix TARGET suffix",
            0,
            0,
        ))));
        tui.start_readerless();
        tui.enable_render_coalescing();

        // The terminal has resized, but input still refers to the old painted
        // frame. Mixing its layout with the live size would clamp to row 0.
        *size.lock().unwrap() = (8, 1);
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), 7),
            (MouseEventKind::Drag(MouseButton::Left), 12),
            (MouseEventKind::Up(MouseButton::Left), 12),
        ] {
            tui.handle_selection_mouse_event(&MouseEvent {
                kind,
                column,
                row: 2,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(tui.selection_text().as_deref(), Some("TARGET"));
        assert!(tui.flush_pending_render());
        assert!(
            tui.selection_text().is_none(),
            "reflow clears the old selection"
        );
    }

    #[test]
    fn reflow_clears_old_selection_instead_of_reusing_stale_columns() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let terminal = ResizableTerminal::new(40, 8);
        let size = terminal.size();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        let scroll = Arc::new(ScrollView::simple(Arc::new(Text::new(
            "prefix TARGET suffix",
            0,
            0,
        ))));
        tui.set_layout_root(Some(scroll));
        tui.start_readerless();
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), 7),
            (MouseEventKind::Drag(MouseButton::Left), 12),
            (MouseEventKind::Up(MouseButton::Left), 12),
        ] {
            tui.handle_selection_mouse_event(&MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(tui.selection_text().as_deref(), Some("TARGET"));
        *size.lock().unwrap() = (30, 8);
        tui.request_render(false);
        assert!(tui.selection_text().is_none());
        assert!(!tui.selection_dragged());
    }

    #[test]
    fn reflow_cancels_a_plain_press_without_leaving_follow_paused() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let terminal = ResizableTerminal::new(40, 8);
        let size = terminal.size();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        let text = Arc::new(Text::new(
            (0..30)
                .map(|i| format!("row {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            0,
            0,
        ));
        let scroll = Arc::new(ScrollView::primary(text));
        tui.set_layout_root(Some(scroll.clone()));
        tui.start_readerless();
        tui.handle_selection_mouse_event(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!scroll.is_following_end());
        *size.lock().unwrap() = (30, 12);
        tui.request_render(false);
        assert!(scroll.is_following_end());
        assert_eq!(scroll.scroll_top(), 18);
        assert!(tui.selection_text().is_none());
        assert!(!tui.handle_selection_mouse_event(&MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE
        }));
    }

    #[test]
    fn a_width_only_resize_repaints_instead_of_patching_reflowed_rows() {
        let terminal = ResizableTerminal::new(40, 8);
        let output = terminal.output();
        let size = terminal.size();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        let editor = Arc::new(Editor::simple());
        editor.set_focused(true);
        tui.set_layout_root(Some(editor));
        tui.start_readerless();
        output.lock().unwrap().clear();

        // Change the terminal's geometry behind the TUI's back so only the
        // renderer's own geometry guard can catch it. The height is unchanged, so
        // a height-only full-redraw check would take the diff path and keep the
        // terminal's reflowed rows: short input borders and repeated spinner
        // rows.
        *size.lock().unwrap() = (52, 8);
        tui.do_render(false);

        let frame = output.lock().unwrap().clone();
        assert!(
            frame.contains("\x1b[2J"),
            "a width change must repaint every row, got: {frame:?}"
        );
        assert_eq!(*tui.painted_size.lock().unwrap(), (52, 8));
    }

    #[test]
    fn a_resize_notification_repaints_even_when_the_window_ends_up_the_same_size() {
        let terminal = ResizableTerminal::new(40, 8);
        let output = terminal.output();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        let editor = Arc::new(Editor::simple());
        editor.set_focused(true);
        tui.set_layout_root(Some(editor));
        tui.start_readerless();
        output.lock().unwrap().clear();

        // Dragged out and back between two watchdog ticks: the size is the one
        // already painted, but the terminal reflowed its rows in between, so a
        // diff against the old frame would leave short borders and repeated
        // spinner rows behind.
        tui.note_resize();

        let frame = output.lock().unwrap().clone();
        assert!(
            frame.contains("\x1b[2J"),
            "a resize notification must repaint every row, got: {frame:?}"
        );
    }

    #[test]
    fn legacy_refresh_size_repaints_a_same_size_resize_with_or_without_coalescing() {
        for coalesced in [false, true] {
            let terminal = ResizableTerminal::new(40, 8);
            let output = terminal.output();
            let tui = TuiAltScreen::new(Box::new(terminal), false, None);
            tui.set_layout_root(Some(Arc::new(Text::new("stable frame", 0, 0))));
            if coalesced {
                tui.enable_render_coalescing();
            }
            tui.start_readerless();
            output.lock().unwrap().clear();
            let redraws = tui.full_redraws();

            // Existing hosts call refresh_size from their Resize handler.
            // An out-and-back drag can reflow the terminal without changing
            // its final dimensions, so the legacy API must force a repaint.
            tui.refresh_size();
            if coalesced {
                assert!(output.lock().unwrap().is_empty());
                assert!(tui.flush_pending_render());
            }
            let frame = output.lock().unwrap().clone();
            assert!(frame.contains("\x1b[2J"), "missing full repaint: {frame:?}");
            assert_eq!(tui.full_redraws(), redraws + 1);

            // Steady-state watchdog ticks must still leave the frame alone.
            output.lock().unwrap().clear();
            tui.ensure_size();
            assert!(!tui.flush_pending_render());
            assert!(output.lock().unwrap().is_empty());
        }
    }

    /// The character grid can stay identical while the cell geometry changes
    /// (font zoom, a window moved to a monitor with another DPI). No resize event
    /// has to arrive for that, so the watchdog is the only place that can notice
    /// it — and a diff against the previous frame would keep the old image rows.
    ///
    /// The previous frame's geometry is faked through `painted_cells` instead of
    /// mutating the process-wide capability cache, which other tests running in
    /// parallel would observe.
    #[test]
    fn the_size_watchdog_repaints_when_only_the_cell_geometry_changed() {
        let terminal = ResizableTerminal::new(40, 8);
        let output = terminal.output();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        tui.set_layout_root(Some(Arc::new(Text::new("stable frame", 0, 0))));
        tui.start_readerless();
        output.lock().unwrap().clear();

        // The frame on screen was laid out for a cell size the terminal no
        // longer reports, with the columns and rows unchanged.
        let painted = *tui.painted_cells.lock().unwrap();
        *tui.painted_cells.lock().unwrap() = CellDimensions {
            width: painted.width.saturating_add(1).max(1),
            height: painted.height.saturating_add(1).max(1),
        };
        let redraws = tui.full_redraws();
        tui.ensure_size();

        let frame = output.lock().unwrap().clone();
        assert!(
            frame.contains("\x1b[2J"),
            "a cell geometry change must repaint every row, got: {frame:?}"
        );
        assert_eq!(tui.full_redraws(), redraws + 1);
        assert_eq!(*tui.painted_cells.lock().unwrap(), painted);

        // A tick that re-reads the same geometry stays free.
        output.lock().unwrap().clear();
        tui.ensure_size();
        assert!(
            output.lock().unwrap().is_empty(),
            "an unchanged cell geometry must not repaint, got: {:?}",
            output.lock().unwrap()
        );
    }

    #[test]
    fn the_size_watchdog_does_not_repaint_an_unchanged_frame() {
        let terminal = ResizableTerminal::new(40, 8);
        let output = terminal.output();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        let editor = Arc::new(Editor::simple());
        editor.set_focused(true);
        tui.set_layout_root(Some(editor));
        tui.start_readerless();
        output.lock().unwrap().clear();

        // A watchdog tick on a steady window must be free: Windows Terminal
        // repeats resize events, and repainting on each one flickers.
        tui.ensure_size();
        assert!(
            output.lock().unwrap().is_empty(),
            "an unchanged size must not repaint, got: {:?}",
            output.lock().unwrap()
        );
    }

    /// Two renderers with different cell geometries must not see each other.
    ///
    /// This is the isolation the previous head lacked: the injected pixel size
    /// and the geometry derived from it were process-wide, so a probe running
    /// in parallel could move another renderer's cell size between two frames.
    /// Each renderer now polls its own terminal, so this holds with no lock at
    /// all — the assertion is that interleaved renders keep their own geometry.
    #[test]
    fn two_renderers_keep_their_own_cell_geometry() {
        let small = ResizableTerminal::new(40, 8);
        let small_pixels = small.pixels();
        *small_pixels.lock().unwrap() = Some((400, 160)); // 10x20 per cell
        let small_out = small.output();
        let small_tui = TuiAltScreen::new(Box::new(small), false, None);
        small_tui.set_layout_root(Some(Arc::new(Text::new("small", 0, 0))));
        small_tui.start_readerless();
        small_out.lock().unwrap().clear();

        let large = ResizableTerminal::new(40, 8);
        let large_pixels = large.pixels();
        *large_pixels.lock().unwrap() = Some((800, 160)); // 20x20 per cell
        let large_out = large.output();
        let large_tui = TuiAltScreen::new(Box::new(large), false, None);
        large_tui.set_layout_root(Some(Arc::new(Text::new("large", 0, 0))));
        large_tui.start_readerless();
        large_out.lock().unwrap().clear();

        assert_eq!(
            *small_tui.cell_geometry.lock().unwrap(),
            Some(CellDimensions {
                width: 10,
                height: 20
            })
        );
        assert_eq!(
            *large_tui.cell_geometry.lock().unwrap(),
            Some(CellDimensions {
                width: 20,
                height: 20
            })
        );

        // Interleave the watchdogs: each renderer re-polls its own terminal and
        // neither adopts the other's geometry.
        for _ in 0..3 {
            small_tui.ensure_size();
            large_tui.ensure_size();
        }
        assert_eq!(
            *small_tui.cell_geometry.lock().unwrap(),
            Some(CellDimensions {
                width: 10,
                height: 20
            }),
            "the smaller-cell renderer must keep its own geometry"
        );
        assert_eq!(
            *large_tui.cell_geometry.lock().unwrap(),
            Some(CellDimensions {
                width: 20,
                height: 20
            }),
            "the larger-cell renderer must keep its own geometry"
        );
    }

    /// A font zoom the terminal reports on its own must reach the watchdog.
    ///
    /// The previous probes faked the *cached* geometry, so they never proved
    /// the poll: this one changes what the terminal answers and asserts the
    /// repaint follows, which is the path a real font zoom takes.
    #[test]
    fn the_watchdog_follows_a_geometry_the_terminal_starts_reporting() {
        let terminal = ResizableTerminal::new(40, 8);
        let pixels = terminal.pixels();
        *pixels.lock().unwrap() = Some((400, 160)); // 10x20 per cell
        let output = terminal.output();
        let tui = TuiAltScreen::new(Box::new(terminal), false, None);
        tui.set_layout_root(Some(Arc::new(Text::new("stable frame", 0, 0))));
        tui.start_readerless();
        output.lock().unwrap().clear();

        // A steady tick stays free.
        tui.ensure_size();
        assert!(
            output.lock().unwrap().is_empty(),
            "an unchanged geometry must not repaint, got: {:?}",
            output.lock().unwrap()
        );

        // Font zoom: same 40x8 grid, half the pixels per cell. No resize event
        // arrives for this, so the watchdog poll is the only thing that can
        // notice.
        *pixels.lock().unwrap() = Some((400, 80));
        let redraws = tui.full_redraws();
        tui.ensure_size();

        let frame = output.lock().unwrap().clone();
        assert!(
            frame.contains("\x1b[2J"),
            "a geometry-only change the terminal reports must repaint every row, got: {frame:?}"
        );
        assert_eq!(tui.full_redraws(), redraws + 1);
        assert_eq!(
            *tui.cell_geometry.lock().unwrap(),
            Some(CellDimensions {
                width: 10,
                height: 10
            }),
            "the watchdog must adopt the geometry the terminal now reports"
        );
    }

    #[test]
    fn changed_rows_overwrite_without_blanking_sidebar_and_clear_shorter_tails_afterwards() {
        for main_screen in [false, true] {
            let output = Arc::new(Mutex::new(String::new()));
            let tui = TuiAltScreen::new(
                Box::new(RecordingTerminal {
                    output: output.clone(),
                }),
                false,
                None,
            );
            let text = Arc::new(Text::new("chat one    │ RUN STATS", 0, 0));
            tui.set_main_screen_mode(main_screen);
            tui.set_layout_root(Some(text.clone()));
            tui.start_readerless();
            output.lock().unwrap().clear();
            text.set_text("chat two    │ RUN STATS");
            tui.do_render(false);
            let frame = output.lock().unwrap().clone();
            assert!(frame.contains("tw"));
            assert!(!frame.contains("RUN STATS"));
            assert!(!frame.contains("\x1b[2K"));
            assert!(!frame.contains("\x1b[2J"));
            output.lock().unwrap().clear();
            text.set_text("short");
            tui.do_render(false);
            let frame = output.lock().unwrap().clone();
            assert!(frame.contains("short"));
            assert!(!frame.contains("\x1b[2K"));
        }
    }

    #[test]
    fn jump_click_resumes_following_and_preserves_the_editor_draft_and_focus() {
        use crate::{StackChild, StackEntry, VStack};
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let tui = TuiAltScreen::new(
            Box::new(RecordingTerminal {
                output: Arc::new(Mutex::new(String::new())),
            }),
            false,
            None,
        );
        let text = Arc::new(Text::new(
            (1..=20)
                .map(|n| format!("message {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            0,
            0,
        ));
        let scroll = Arc::new(ScrollView::primary(text.clone()));
        let editor = Arc::new(Editor::simple());
        editor.set_text("未发送的草稿");
        let root = Arc::new(VStack::from_children(vec![
            StackChild::Entry(StackEntry::new(scroll.clone()).basis(0).grow(1).min_size(1)),
            StackChild::Entry(StackEntry::new(editor.clone())),
        ]));
        tui.set_layout_root(Some(root));
        tui.set_focus(Some(editor.clone()));
        tui.set_scroll_to_end_indicator(Some(Arc::new(|| {
            " ↓ Jump to latest message · End ".to_owned()
        })));
        tui.start_readerless();
        tui.scroll_by(-4);
        assert!(!scroll.is_following_end());
        assert!(tui
            .previous_screen
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("Jump to latest")));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 20,
            row: (scroll.viewport_height() - 1) as u16,
            modifiers: KeyModifiers::NONE,
        };
        assert!(tui.handle_scroll_to_end_indicator_mouse_event(&click));
        assert!(scroll.is_following_end());
        assert!(!tui
            .previous_screen
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("Jump to latest")));
        assert_eq!(editor.get_text(), "未发送的草稿");
        let focused: Arc<dyn Component> = editor.clone();
        assert!(Arc::ptr_eq(&tui.get_focus().unwrap(), &focused));
        text.set_text(
            (1..=24)
                .map(|n| format!("message {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        tui.do_render(false);
        assert_eq!(
            scroll.scroll_top(),
            scroll.content_height() - scroll.viewport_height()
        );
        assert!(tui
            .previous_screen
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("message 24")));
        tui.stop(Default::default());
    }

    struct RecordingTerminal {
        output: Arc<Mutex<String>>,
    }

    impl Terminal for RecordingTerminal {
        fn info(&self) -> TerminalInfo {
            TerminalInfo {
                columns: 40,
                rows: 8,
                ..Default::default()
            }
        }

        fn write(&self, data: &str) {
            self.output.lock().unwrap().push_str(data);
        }

        fn hide_cursor(&self) {}
        fn show_cursor(&self) {}
        fn move_cursor(&self, _row: usize, _col: usize) {}
        fn clear_screen(&self) {}
        fn set_title(&self, _title: &str) {}
        fn enable_mouse(&self) {}
        fn disable_mouse(&self) {}
        fn enter_raw_mode(&self) {
            self.write("<raw-on>");
        }
        fn exit_raw_mode(&self) {
            self.write("<raw-off>");
        }
        fn refresh_size(&self) {}
        fn start(
            &self,
            _on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
            _on_resize: Box<dyn Fn() + Send + Sync>,
        ) {
            self.enter_raw_mode();
        }
        fn stop(&self) {
            self.write("<stop>");
        }
        fn is_tty(&self) -> bool {
            true
        }
        fn set_progress(&self, _active: bool) {}
        fn flush(&self) {}
    }

    #[test]
    fn keyboard_setup_and_teardown_stay_on_the_owning_screen_buffer() {
        for readerless in [true, false] {
            let output = Arc::new(Mutex::new(String::new()));
            let tui = TuiAltScreen::new(
                Box::new(RecordingTerminal {
                    output: output.clone(),
                }),
                true,
                None,
            );
            if readerless {
                tui.start_readerless();
            } else {
                tui.start();
            }
            let rendered = output.lock().unwrap().clone();
            assert!(rendered.find("\x1b[?1049h").unwrap() < rendered.find("<raw-on>").unwrap());
            output.lock().unwrap().clear();
            tui.suspend();
            let rendered = output.lock().unwrap().clone();
            assert!(rendered.find("<raw-off>").unwrap() < rendered.find("\x1b[?1049l").unwrap());
            assert!(rendered.contains("\x1b[?7h"));
            output.lock().unwrap().clear();
            tui.resume();
            let rendered = output.lock().unwrap().clone();
            assert!(rendered.find("\x1b[?1049h").unwrap() < rendered.find("<raw-on>").unwrap());
            output.lock().unwrap().clear();
            tui.stop(TuiStopOptions::default());
            let rendered = output.lock().unwrap().clone();
            assert!(rendered.find("<stop>").unwrap() < rendered.find("\x1b[?1049l").unwrap());
        }
    }

    #[test]
    fn regular_start_never_switches_to_the_alternate_screen() {
        let output = Arc::new(Mutex::new(String::new()));
        let tui = TuiAltScreen::new(
            Box::new(RecordingTerminal {
                output: output.clone(),
            }),
            true,
            None,
        );
        tui.set_main_screen_mode(true);
        tui.start();
        tui.suspend();
        tui.resume();
        tui.stop(TuiStopOptions::default());
        let rendered = output.lock().unwrap().clone();
        assert!(rendered.contains("<raw-on>"));
        assert!(!rendered.contains("\x1b[?1049"));
    }

    #[test]
    fn cursor_marker_is_never_written_to_the_terminal() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        let editor = Arc::new(Editor::simple());
        editor.set_focused(true);
        editor.insert("hello");
        tui.set_layout_root(Some(editor.clone()));
        tui.start_readerless();

        editor.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        tui.request_render(false);

        let rendered = output.lock().unwrap().clone();
        assert!(!rendered.contains(CURSOR_MARKER));
        assert!(rendered.contains("hello"));
        assert_eq!(editor.get_text(), "hello");
    }

    #[test]
    fn layout_does_not_touch_main_screen_and_stop_clears_it() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);

        tui.set_layout_root(Some(Arc::new(Text::new("screen content", 0, 0))));
        assert!(output.lock().unwrap().is_empty());

        tui.start_readerless();
        assert!(output.lock().unwrap().contains("screen content"));

        tui.stop(TuiStopOptions::default());
        let rendered = output.lock().unwrap().clone();
        let content_position = rendered.find("screen content").unwrap();
        let clear_position = rendered
            .rfind("\x1b[?1049l\x1b[2J\x1b[H\x1b[?25h")
            .expect("stop should leave alt screen and clear the restored main screen");
        assert!(clear_position > content_position);
    }

    #[test]
    fn cached_redraw_is_suppressed_while_rendering_is_suspended() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        tui.set_layout_root(Some(Arc::new(Text::new("outer frame", 0, 0))));
        tui.start_readerless();

        tui.set_render_suspended(true);
        let before = output.lock().unwrap().len();
        tui.request_render_reusing_scroll_content();
        assert_eq!(output.lock().unwrap().len(), before);

        tui.set_render_suspended(false);
        tui.set_layout_root(Some(Arc::new(Text::new("new frame", 0, 0))));
        assert!(output.lock().unwrap().len() > before);
    }

    #[test]
    fn regular_single_line_update_does_not_rewrite_unchanged_tail() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        let content = Arc::new(Text::new("header\nstatus\neditor\nfooter", 0, 0));
        tui.set_main_screen_mode(true);
        tui.set_layout_root(Some(content.clone()));
        tui.start_readerless();
        output.lock().unwrap().clear();

        content.set_text("header\nstatus2\neditor\nfooter");
        tui.request_render(false);

        let rendered = output.lock().unwrap().clone();
        assert!(rendered.contains("\x1b[6C2"));
        assert!(!rendered.contains("editor"));
        assert!(!rendered.contains("footer"));
    }

    #[test]
    fn regular_unchanged_frame_writes_nothing() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        tui.set_main_screen_mode(true);
        tui.set_layout_root(Some(Arc::new(Text::new(
            "header\nstatus\neditor\nfooter",
            0,
            0,
        ))));
        tui.start_readerless();
        output.lock().unwrap().clear();

        tui.request_render(false);

        assert!(output.lock().unwrap().is_empty());
    }

    /// The main-screen renderer must poll the cell geometry before it lays the
    /// document out, not just inherit whatever `ensure_size` last wrote.
    ///
    /// The alt-screen path already re-polls right before layout; the main
    /// screen returned to `do_render_main_screen()` first, so a host that
    /// renders without a periodic watchdog would keep the geometry from
    /// startup.
    ///
    /// The pixel size comes from the fake terminal's own `window_size()`, so
    /// this test shares no mutable state with the rest of the binary: nothing
    /// it does can move another renderer's geometry, and nothing another test
    /// does can move this one's. The assertions read the renderer's polled
    /// geometry rather than the process-wide cache for the same reason.
    #[test]
    fn the_main_screen_polls_the_cell_geometry_before_laying_out() {
        // The grid is 40x8; 400x160 pixels means one cell is 10x20.
        let terminal = ResizableTerminal::new(40, 8);
        let pixels = terminal.pixels();
        *pixels.lock().unwrap() = Some((400, 160));
        let output = terminal.output();
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        tui.set_main_screen_mode(true);
        tui.set_layout_root(Some(Arc::new(Text::new("main screen frame", 0, 0))));
        tui.start_readerless();
        output.lock().unwrap().clear();

        // `start_readerless` paints the first frame; the renderer must have
        // polled the pixel size instead of trusting the 8x16 default.
        assert_eq!(
            *tui.cell_geometry.lock().unwrap(),
            Some(CellDimensions {
                width: 10,
                height: 20
            }),
            "the main-screen renderer must poll the cell geometry before layout"
        );

        // A later font zoom (new pixel size, same grid) must be picked up by a
        // plain render too, without waiting for `ensure_size`.
        *pixels.lock().unwrap() = Some((800, 160));
        tui.do_render(false);
        assert_eq!(
            *tui.cell_geometry.lock().unwrap(),
            Some(CellDimensions {
                width: 20,
                height: 20
            }),
            "a direct main-screen render must re-poll the cell geometry"
        );

        // The Windows / zero-pixel fallback: a terminal that cannot report a
        // pixel size must leave the last known geometry in place instead of
        // resetting it to the 8x16 default mid-session.
        *pixels.lock().unwrap() = None;
        tui.do_render(false);
        assert_eq!(
            *tui.cell_geometry.lock().unwrap(),
            Some(CellDimensions {
                width: 20,
                height: 20
            }),
            "an unsupported pixel query must keep the polled geometry"
        );
    }

    #[test]
    fn merge_pending_render_never_downgrades_a_full_render() {
        // Nothing parked: the request's own variant wins.
        assert_eq!(merge_pending_render(None, false), Some(false));
        assert_eq!(merge_pending_render(None, true), Some(true));
        // A parked cached-content frame is upgraded by a full render...
        assert_eq!(merge_pending_render(Some(true), false), Some(false));
        // ...and stays cached when another cached request arrives.
        assert_eq!(merge_pending_render(Some(true), true), Some(true));
        // A parked full render is never downgraded by a cached request.
        assert_eq!(merge_pending_render(Some(false), true), Some(false));
        assert_eq!(merge_pending_render(Some(false), false), Some(false));
    }

    #[test]
    fn rapid_requests_coalesce_into_a_single_frame() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        let content = Arc::new(Text::new("first", 0, 0));
        tui.set_layout_root(Some(content.clone()));
        tui.enable_render_coalescing();
        tui.start_readerless();
        output.lock().unwrap().clear();

        // A streaming burst: many requests, no painting until the host flush.
        content.set_text("second");
        for _ in 0..50 {
            tui.request_render(false);
        }
        assert!(
            output.lock().unwrap().is_empty(),
            "parked requests must not repaint the transcript"
        );

        // The host flush paints exactly one frame and clears what was owed.
        assert!(tui.flush_pending_render());
        let rendered = output.lock().unwrap().clone();
        assert!(rendered.contains("second"));
        assert!(!tui.flush_pending_render(), "nothing owed after the flush");
    }

    #[test]
    fn immediate_render_discards_a_parked_frame() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        let content = Arc::new(Text::new("before", 0, 0));
        tui.set_layout_root(Some(content.clone()));
        tui.enable_render_coalescing();
        tui.start_readerless();
        output.lock().unwrap().clear();

        content.set_text("after");
        tui.request_render(false);
        // An immediate frame (keyboard input, selector open) wins over the
        // parked one instead of rendering the same content twice.
        tui.render_now(true);

        assert!(output.lock().unwrap().contains("after"));
        assert!(!tui.flush_pending_render());
    }

    /// End-to-end throttle check on the real scheduler thread. Each frame emits
    /// exactly one synchronized-output opener (`\x1b[?2026h`), so counting those
    /// counts painted frames. Before the throttle, every one of the 200
    /// "streamed deltas" repainted the whole transcript.
    #[test]
    fn streaming_burst_paints_at_most_one_frame_per_interval() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = Arc::new(TuiAltScreen::new(Box::new(terminal), true, None));
        let content = Arc::new(Text::new("0", 0, 0));
        tui.set_layout_root(Some(content.clone()));
        tui.start_render_scheduler();
        tui.start_readerless();
        output.lock().unwrap().clear();

        for i in 0..200 {
            content.set_text(&format!("delta {i}"));
            tui.request_render(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(50));

        let frames = output.lock().unwrap().matches("\x1b[?2026h").count();
        assert!(frames >= 1, "the scheduler must still paint frames");
        assert!(
            frames <= 40,
            "200 requests in ~250 ms must coalesce into ~16 ms frames, painted {frames}"
        );
        tui.stop(TuiStopOptions::default());
    }

    /// A transcript row that holds a Kitty placement, plus enough rows around
    /// it to scroll it in and out of the viewport.
    struct ImageTranscript {
        lines: Vec<String>,
    }

    impl Component for ImageTranscript {
        fn render(&self, _width: usize) -> Vec<String> {
            self.lines.clone()
        }
        fn invalidate(&self) {}
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    const IMAGE_ID: u32 = 9_902;

    /// A transcript whose row 2 carries a 4-row Kitty placement with a payload
    /// large enough that re-sending it would be obvious in the frame bytes.
    fn image_screen() -> (TuiAltScreen, Arc<Mutex<String>>, Arc<ScrollView>) {
        let mut payload = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        payload.resize(44_181, 0);
        let placement = crate::terminal_image::encode_kitty(
            &payload,
            &crate::terminal_image::ImageRenderOptions {
                id: Some(IMAGE_ID),
                width: Some(20),
                height: Some(4),
                ..Default::default()
            },
        );
        crate::terminal_image::register_kitty_image(crate::terminal_image::KittyImageMetadata {
            id: IMAGE_ID,
            columns: 20,
            rows: 4,
            width_px: 400,
            height_px: 320,
        });

        let mut lines: Vec<String> = (0..40).map(|row| format!("row {row}")).collect();
        lines[2] = placement;
        for row in &mut lines[3..6] {
            row.clear();
        }

        let scroll = Arc::new(ScrollView::new(
            Arc::new(ImageTranscript { lines }),
            ScrollViewOptions {
                follow: crate::FollowMode::None,
                ..Default::default()
            },
        ));
        let stack = VStack::new();
        stack.add_child_with_options(scroll.clone(), StackEntryOptions::with_basis(8));

        let output = Arc::new(Mutex::new(String::new()));
        let tui = TuiAltScreen::new(
            Box::new(RecordingTerminal {
                output: output.clone(),
            }),
            false,
            None,
        );
        tui.set_layout_root(Some(Arc::new(stack)));
        tui.start_readerless();
        (tui, output, scroll)
    }

    /// Scrolling an image repaints its row. The row must move the placement by
    /// id instead of re-sending the payload, and it must not delete the image
    /// data the placement-only rewrite depends on.
    #[test]
    fn scrolling_an_image_moves_its_placement_without_resending_the_payload() {
        let (tui, output, scroll) = image_screen();
        let first = output.lock().unwrap().clone();
        assert!(
            first.contains("\x1b_Ga=T,") && first.contains("i=9902,p=1"),
            "the first frame must transmit the payload, got {} bytes",
            first.len()
        );

        output.lock().unwrap().clear();
        scroll.scroll_to(1);
        tui.do_render(false);
        let frame = output.lock().unwrap().clone();

        assert!(
            frame.contains("\x1b_Ga=p,") && frame.contains("i=9902,p=1"),
            "the moved image must be placed by id, got {frame:?}"
        );
        assert!(
            !frame.contains("iVBORw0KGgo"),
            "the payload must not be re-sent, got {} bytes",
            frame.len()
        );
        assert!(
            !frame.contains("\x1b_Ga=d,"),
            "an image that is still on screen must be moved, not deleted: {frame:?}"
        );
    }

    /// An image that scrolls out of the viewport is deleted by placement only:
    /// the stored data has to survive for a later re-place of the same id.
    #[test]
    fn an_image_that_scrolls_off_screen_is_deleted_by_placement_only() {
        let (tui, output, scroll) = image_screen();
        output.lock().unwrap().clear();

        // The placement covers rows 2..6; a viewport starting at row 8 no
        // longer contains it.
        scroll.scroll_to(8);
        tui.do_render(false);
        let frame = output.lock().unwrap().clone();

        assert!(
            frame.contains("\x1b_Ga=d,d=i,i=9902,p=1"),
            "the off-screen placement must be deleted, got {frame:?}"
        );
        assert!(
            !frame.contains("d=I"),
            "deleting the image data would blank a later re-place: {frame:?}"
        );
    }
}
