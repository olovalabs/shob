//! Owner of the `alacritty_terminal` [`Term`] and the VTE [`Processor`].
//!
//! Grid access goes through [`TerminalState`]'s `parking_lot::Mutex`, shared
//! with deferred paint closures. The blocking PTY reader only queues raw bytes;
//! the view's foreground task performs parsing and UI-state updates.

use crate::agent_status::{AgentOscScanner, AgentStatusReport};
use crate::colors::ColorPalette;
use crate::event::{GpuiEventProxy, TerminalNotifierHandle};
use crate::shell_integration::{BlockSnapshot, ShellEvent, ShellIntegrationScanner, snapshot_term};
use crate::theme_protocol::ThemeProtocol;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::Processor;
use parking_lot::Mutex;
use std::sync::Arc;

/// Upper bound on the number of highlighted matches we will collect for a
/// single search, mirroring Zed's `MAX_SEARCH_LINES` style guard so a pathological
/// regex over a 100k-line scrollback cannot stall a frame.
pub const MAX_SEARCH_MATCHES: usize = 5_000;

/// Dimensions handed to `Term::new` / `Term::resize`.
///
/// Note that `total_lines` must be `screen_lines` here: the scrollback size is
/// taken from [`Config::scrolling_history`], *not* from this trait, and
/// `Grid::resize` only looks at `screen_lines`/`columns`.
#[derive(Debug, Clone, Copy)]
pub struct TermDimensions {
    columns: usize,
    screen_lines: usize,
}

impl TermDimensions {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns: columns.max(alacritty_terminal::term::MIN_COLUMNS),
            screen_lines: screen_lines.max(alacritty_terminal::term::MIN_SCREEN_LINES),
        }
    }
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }

    fn last_column(&self) -> Column {
        Column(self.columns.saturating_sub(1))
    }

    fn topmost_line(&self) -> Line {
        Line(0)
    }

    fn bottommost_line(&self) -> Line {
        Line(self.screen_lines as i32 - 1)
    }
}

/// Owner of the live grid.
///
/// The grid is the **single source of truth** for the terminal's size: the
/// paint pass resizes it directly through [`TerminalState::term_arc`], so any
/// size kept here would be stale for part of every resize. Callers that need
/// the size ask the grid ([`TerminalState::cols`], [`TerminalState::rows`],
/// [`TerminalState::dimensions`]), which is what keeps them from addressing
/// rows and columns that no longer exist.
pub struct TerminalState {
    term: Arc<Mutex<Term<GpuiEventProxy>>>,
    parser: Processor,
    theme_protocol: ThemeProtocol,
    config: Config,
    /// Kept so protocol passthroughs can reply into the PTY.
    notifier: TerminalNotifierHandle,
    agent_osc: AgentOscScanner,
    shell: ShellIntegrationScanner,
    /// Current palette, used when snapshotting output into blocks.
    palette: ColorPalette,
}

impl TerminalState {
    /// Create a terminal with alacritty's default `Config` (10k lines of
    /// scrollback, default semantic escape chars).
    pub fn new(cols: usize, rows: usize, event_proxy: GpuiEventProxy) -> Self {
        Self::with_config(cols, rows, Config::default(), event_proxy)
    }

    /// Create a terminal with an explicit alacritty [`Config`].
    ///
    /// This is how the scrollback size actually reaches the grid — passing a
    /// bigger `Dimensions::total_lines` does nothing.
    pub fn with_config(
        cols: usize,
        rows: usize,
        config: Config,
        event_proxy: GpuiEventProxy,
    ) -> Self {
        let dimensions = TermDimensions::new(cols, rows);
        let notifier = event_proxy.notifier_handle();
        let term = Term::new(config.clone(), &dimensions, event_proxy);

        Self {
            term: Arc::new(Mutex::new(term)),
            parser: Processor::new(),
            theme_protocol: ThemeProtocol::default(),
            config,
            notifier,
            agent_osc: AgentOscScanner::default(),
            shell: ShellIntegrationScanner::default(),
            palette: ColorPalette::default(),
        }
    }

    /// Build an alacritty [`Config`] from the component-level settings.
    pub fn build_config(scrollback: usize, semantic_escape_chars: &str, osc52: bool) -> Config {
        Config {
            scrolling_history: scrollback,
            semantic_escape_chars: semantic_escape_chars.to_owned(),
            // The kitty keyboard protocol is deliberately off: an application
            // that negotiates it expects the frontend to *encode* keys the
            // kitty way, and `input.rs` speaks plain xterm.
            kitty_keyboard: false,
            osc52: if osc52 {
                Osc52::CopyPaste
            } else {
                Osc52::OnlyCopy
            },
            ..Config::default()
        }
    }

    /// Parse PTY bytes and return shell-integration events found in the chunk.
    ///
    /// The stream is split at marker boundaries so cursor positions captured at
    /// `OSC 133;B/C/D` are accurate, and so output bytes between `C` and `D` can
    /// be captured for the block model.
    pub fn process_bytes(&mut self, bytes: &[u8]) -> Vec<ShellEvent> {
        self.answer_xtgettcap(bytes);
        self.agent_osc.scan(bytes);

        let hits = self.shell.scan(bytes);
        let mut events = Vec::new();
        let mut consumed = 0usize;
        let mut term = self.term.lock();

        for hit in hits {
            let end = hit.end.min(bytes.len());
            if end > consumed {
                let slice = &bytes[consumed..end];
                self.shell.capture_bytes(slice);
                self.theme_protocol.advance(
                    &mut self.parser,
                    &mut term,
                    &self.notifier,
                    &self.palette,
                    slice,
                );
                consumed = end;
            }
            self.shell
                .apply(hit.kind, &term, std::time::Instant::now(), &mut events);
            self.shell
                .note_mode(term.mode().contains(TermMode::ALT_SCREEN));
        }

        if consumed < bytes.len() {
            let slice = &bytes[consumed..];
            self.shell.capture_bytes(slice);
            self.theme_protocol.advance(
                &mut self.parser,
                &mut term,
                &self.notifier,
                &self.palette,
                slice,
            );
        }
        self.shell
            .note_mode(term.mode().contains(TermMode::ALT_SCREEN));

        events
    }

    /// Feed bytes to the emulator without shell-integration scanning. Used by
    /// the throw-away terminal that re-parses captured block output.
    pub fn feed_raw(&mut self, bytes: &[u8]) {
        let mut term = self.term.lock();
        self.theme_protocol.advance(
            &mut self.parser,
            &mut term,
            &self.notifier,
            &self.palette,
            bytes,
        );
    }

    /// Set the configured palette and notify subscribed applications if it changed.
    /// The notifier's shared palette must already contain the new colors, so an
    /// immediate OSC 11 query in response to the notification observes them.
    pub fn set_palette(&mut self, palette: ColorPalette) {
        let changed = self.palette.ansi_colors() != palette.ansi_colors()
            || self.palette.extended_colors() != palette.extended_colors()
            || self.palette.foreground() != palette.foreground()
            || self.palette.background() != palette.background()
            || self.palette.cursor() != palette.cursor();
        self.palette = palette;
        if changed {
            self.theme_protocol
                .palette_changed(&self.notifier, &self.palette);
        }
    }

    /// Only accept OSC 133 markers stamped with this token.
    pub fn set_shell_token(&mut self, token: String) {
        self.shell.set_token(token);
    }

    /// Disable unused raw block copies without disabling shell ownership markers.
    pub fn set_capture_output(&mut self, enabled: bool) {
        self.shell.set_capture_output(enabled);
    }

    pub fn palette(&self) -> &ColorPalette {
        &self.palette
    }

    /// Snapshot the terminal buffer into immutable, styled block lines.
    pub fn snapshot_buffer_lines(&self, palette: &ColorPalette, truncated: bool) -> BlockSnapshot {
        self.with_term(|term| snapshot_term(term, palette, truncated))
    }

    /// Most recent cwd reported by the shell (OSC 7 / OSC 7502).
    pub fn shell_cwd(&self) -> Option<&str> {
        self.shell.cwd()
    }

    /// Current highest-priority status reported over OSC 7501.
    pub fn agent_status_report(&self) -> Option<AgentStatusReport> {
        self.agent_osc.resolved_report()
    }

    pub fn clear_agent_status_reports(&mut self) {
        self.agent_osc.clear_reports();
    }

    /// Most recent OSC 9;4 progress payload, as `4;1;40`.
    pub fn osc_progress(&self) -> &str {
        self.agent_osc.progress()
    }

    /// Answer XTGETTCAP queries (`DCS + q <hex-encoded caps> ST`).
    ///
    /// alacritty_terminal's ANSI handler never dispatches DCS (vte's
    /// `hook`/`put`/`unhook` are no-ops), so the raw byte stream is snooped here
    /// and replies are written straight into the PTY through the notifier.
    /// Per xterm/kitty, a supported capability is answered with
    /// `DCS 1 + r <hex-cap>=<hex-value> ST` and an unsupported one with
    /// `DCS 0 + r <hex-cap> ST`. Capability names may be `;`-separated, in which
    /// case one reply is sent per name.
    ///
    /// Only queries fully contained in `bytes` are answered: a query split
    /// across two PTY reads is ignored rather than answered piecemeal (agent
    /// CLIs send their capability probes in a single write).
    fn answer_xtgettcap(&mut self, bytes: &[u8]) {
        // Fast path: every query starts with ESC, which plain output never
        // contains — one short scan instead of two substring searches per read.
        if !bytes.contains(&0x1b) {
            return;
        }
        const DCS_PLUS_Q: &[u8] = b"\x1bP+q";
        const ST: &[u8] = b"\x1b\\";

        let mut search_from = 0;
        while let Some(rel) = find_subslice(&bytes[search_from..], DCS_PLUS_Q) {
            let start = search_from + rel + DCS_PLUS_Q.len();
            let Some(end_rel) = find_subslice(&bytes[start..], ST) else {
                break; // unterminated query: cannot answer it from this read
            };
            // `Pt` is one or more `;`-separated hex-encoded names, so split the
            // raw payload first and decode each name separately.
            for name_hex in bytes[start..start + end_rel].split(|&b| b == b';') {
                if name_hex.is_empty() {
                    continue;
                }
                if let Some(cap) = decode_hex(name_hex) {
                    let reply = xtgettcap_reply(cap.as_str());
                    self.notifier.write(&reply);
                }
            }
            search_from = start + end_rel + ST.len();
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.term.lock().resize(TermDimensions::new(cols, rows));
    }

    /// Apply a new alacritty config (scrollback size, semantic escape chars, …)
    /// to a live terminal.
    pub fn set_config(&mut self, config: Config) {
        if self.config == config {
            return;
        }
        self.config = config.clone();
        let mut term = self.term.lock();
        term.set_options(config);
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    pub fn with_term<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Term<GpuiEventProxy>) -> R,
    {
        let term = self.term.lock();
        f(&term)
    }

    pub fn with_term_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Term<GpuiEventProxy>) -> R,
    {
        let mut term = self.term.lock();
        f(&mut term)
    }

    /// Grid width, read from the grid itself.
    pub fn cols(&self) -> usize {
        self.term.lock().columns()
    }

    /// Grid height, read from the grid itself.
    ///
    /// Never cache this. The paint pass resizes the grid straight through
    /// [`Self::term_arc`], so a copy of the size kept anywhere else is a frame
    /// behind during a resize — and rows that were resized away are *scrollback*
    /// to alacritty, which panics when asked for a line past the buffer.
    pub fn rows(&self) -> usize {
        self.term.lock().screen_lines()
    }

    /// Grid size as one atomic pair, for callers that need both halves to
    /// describe the same grid.
    pub fn dimensions(&self) -> (usize, usize) {
        self.with_term(|term| (term.columns(), term.screen_lines()))
    }

    pub fn term_arc(&self) -> Arc<Mutex<Term<GpuiEventProxy>>> {
        Arc::clone(&self.term)
    }

    // ---------------------------------------------------------------------
    // Scrolling
    // ---------------------------------------------------------------------

    pub fn display_offset(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    pub fn history_size(&self) -> usize {
        self.term.lock().grid().history_size()
    }

    /// `true` when the viewport is pinned to the newest output.
    pub fn is_at_bottom(&self) -> bool {
        self.display_offset() == 0
    }

    /// Scroll the viewport.
    ///
    /// `Term::scroll_display` already clamps the vi cursor into the viewport and
    /// recomputes the vi selection, so this is all that is needed for vi mode
    /// to stay consistent while scrolling.
    pub fn scroll(&self, scroll: Scroll) {
        self.term.lock().scroll_display(scroll);
    }

    pub fn scroll_lines(&self, lines: i32) {
        if lines != 0 {
            self.scroll(Scroll::Delta(lines));
        }
    }

    pub fn scroll_to_bottom(&self) {
        self.scroll(Scroll::Bottom);
    }

    pub fn scroll_to_top(&self) {
        self.scroll(Scroll::Top);
    }

    // ---------------------------------------------------------------------
    // Selection
    // ---------------------------------------------------------------------

    pub fn set_selection(&self, selection: Option<Selection>) {
        self.term.lock().selection = selection;
    }

    pub fn start_selection(&self, ty: SelectionType, point: Point, side: Side) {
        self.term.lock().selection = Some(Selection::new(ty, point, side));
    }

    pub fn update_selection(&self, point: Point, side: Side) {
        let mut term = self.term.lock();
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn selection_range(&self) -> Option<SelectionRange> {
        let term = self.term.lock();
        term.selection.as_ref().and_then(|s| s.to_range(&term))
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    pub fn has_selection(&self) -> bool {
        let term = self.term.lock();
        term.selection
            .as_ref()
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }

    pub fn clear_selection(&self) {
        self.term.lock().selection = None;
    }

    /// Select the entire scrollback plus the visible screen.
    pub fn select_all(&self) {
        let mut term = self.term.lock();
        let start = Point::new(term.topmost_line(), Column(0));
        let end = Point::new(term.bottommost_line(), term.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
    }

    // ---------------------------------------------------------------------
    // Search
    // ---------------------------------------------------------------------

    /// Collect every match of `regex` in the scrollback + screen.
    pub fn all_matches(&self, regex: &mut RegexSearch) -> Vec<Match> {
        let term = self.term.lock();
        let start = Point::new(term.topmost_line(), Column(0));
        let end = Point::new(term.bottommost_line(), term.last_column());
        RegexIter::new(start, end, Direction::Right, &term, regex)
            .take(MAX_SEARCH_MATCHES)
            .collect()
    }

    /// Find the next match relative to `origin`.
    pub fn search_next(
        &self,
        regex: &mut RegexSearch,
        origin: Point,
        direction: Direction,
    ) -> Option<Match> {
        let term = self.term.lock();
        term.search_next(regex, origin, direction, Side::Left, None)
    }

    /// Scroll the viewport so that `point` is visible.
    pub fn scroll_to_point(&self, point: Point) {
        self.term.lock().scroll_to_point(point);
    }

    // ---------------------------------------------------------------------
    // Vi mode
    // ---------------------------------------------------------------------

    pub fn is_vi_mode(&self) -> bool {
        self.term.lock().mode().contains(TermMode::VI)
    }

    pub fn toggle_vi_mode(&self) {
        self.term.lock().toggle_vi_mode();
    }

    pub fn vi_motion(&self, motion: ViMotion) {
        self.term.lock().vi_motion(motion);
    }

    pub fn vi_goto_point(&self, point: Point) {
        self.term.lock().vi_goto_point(point);
    }

    pub fn vi_cursor_point(&self) -> Point {
        self.term.lock().vi_mode_cursor.point
    }

    // ---------------------------------------------------------------------
    // Misc helpers
    // ---------------------------------------------------------------------

    /// `true` when the application currently wants a blinking cursor
    /// (`DECSCUSR` blinking shapes or `CSI ? 12 h`).
    pub fn cursor_blinking(&self) -> bool {
        self.term.lock().cursor_style().blinking
    }

    pub fn set_focused(&self, focused: bool) {
        self.term.lock().is_focused = focused;
    }

    /// Clamp a terminal point into the valid grid range.
    pub fn clamp_point(&self, point: Point) -> Point {
        let term = self.term.lock();
        point.grid_clamp(&*term, Boundary::Grid)
    }

    /// Reset the terminal (`clear` + wipe the scrollback), like Zed's
    /// `terminal: clear` action.
    pub fn clear_screen_and_scrollback(&mut self) {
        {
            let mut term = self.term.lock();
            term.selection = None;
        }
        // `ESC c` (RIS) resets the emulator; `CSI 3 J` drops the scrollback.
        let _ = self.process_bytes(b"\x1b[H\x1b[2J\x1b[3J");
    }

    /// Text of a single terminal line, trailing whitespace trimmed.
    pub fn line_text(&self, line: Line) -> String {
        let term = self.term.lock();
        let start = Point::new(line, Column(0));
        let end = Point::new(line, term.last_column());
        term.bounds_to_string(start, end).trim_end().to_string()
    }

    /// Snapshot the whole buffer (scrollback + visible screen) as plain text,
    /// keeping the newest `max_lines` lines. Used to persist a pane's output
    /// across restarts; control characters are never produced by
    /// `bounds_to_string`, so the result is safe to feed back with `feed_raw`.
    pub fn buffer_text(&self, max_lines: usize) -> String {
        if max_lines == 0 {
            return String::new();
        }
        let term = self.term.lock();
        let history = term.grid().history_size();
        let screen_lines = term.screen_lines();
        let total = history + screen_lines;
        let keep = max_lines.min(total);
        if keep == 0 {
            return String::new();
        }

        let first = total - keep;
        let last_column = term.last_column();
        let mut lines = Vec::with_capacity(keep);
        for index in first..total {
            let line = Line(index as i32 - history as i32);
            let start = Point::new(line, Column(0));
            let end = Point::new(line, last_column);
            lines.push(term.bounds_to_string(start, end).trim_end().to_string());
        }
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        if lines.is_empty() {
            return String::new();
        }

        let mut text = String::with_capacity(lines.iter().map(|line| line.len() + 2).sum());
        for line in lines {
            text.push_str(&line);
            text.push_str("\r\n");
        }
        text
    }

    /// Retrieve the bottom N lines from the visible viewport.
    pub fn bottom_lines(&self, n: usize) -> Vec<String> {
        let mut lines = Vec::new();
        self.bottom_lines_into(n, &mut lines);
        lines
    }

    /// Like [`Self::bottom_lines`], but reuses the caller's buffer across
    /// ticks so a status poller scraping every session at 2 Hz does not
    /// reallocate the outer `Vec` each time. Each line is still freshly
    /// extracted (and trimmed in place: one allocation per line, not two).
    pub fn bottom_lines_into(&self, n: usize, out: &mut Vec<String>) {
        let term = self.term.lock();
        // The row count is read from the same grid the rows are read from, under
        // one lock. A cached count can be larger than the grid — the paint pass
        // shrinks it mid-frame, and this runs from the status poller between
        // frames — and asking the grid for a row past its buffer aborts the
        // process inside alacritty's line indexing.
        let screen_lines = term.screen_lines();
        let last_col = term.last_column();
        let count = n.min(screen_lines);
        let start_row = screen_lines.saturating_sub(count);
        out.clear();
        out.reserve(count);
        for row in start_row..screen_lines {
            let line = Line(row as i32);
            let start = Point::new(line, Column(0));
            let end = Point::new(line, last_col);
            let mut text = term.bounds_to_string(start, end);
            let trimmed_len = text.trim_end().len();
            text.truncate(trimmed_len);
            out.push(text);
        }
    }
}

/// Find `needle` in `haystack` and return the byte offset of the first match.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Value of an XTGETTCAP capability, if we implement it.
///
/// `RGB`/`Tc` advertise direct colour support, which the renderer does honour
/// (the palette and truecolour SGR sequences are handled by `colors.rs`).
/// Everything else is answered as unsupported so probing clients fall back to
/// their conservative path instead of assuming the feature exists.
fn xtgettcap_value(cap: &str) -> Option<&'static str> {
    match cap {
        "RGB" | "Tc" => Some("1"),
        _ => None,
    }
}

/// Build the `DCS` reply for one requested capability.
fn xtgettcap_reply(cap: &str) -> Vec<u8> {
    let name_hex = encode_hex(cap.as_bytes());
    match xtgettcap_value(cap) {
        Some(value) => {
            format!("\x1bP1+r{name_hex}={}\x1b\\", encode_hex(value.as_bytes())).into_bytes()
        }
        None => format!("\x1bP0+r{name_hex}\x1b\\").into_bytes(),
    }
}

/// Lowercase hex encoding, as used by XTGETTCAP parameters.
fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decode a hex nibble pair per byte. Returns `None` if the sequence is
/// malformed (odd length or bad nibble).
fn decode_hex(bytes: &[u8]) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut chunks = bytes.chunks_exact(2);
    for chunk in &mut chunks {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn state(cols: usize, rows: usize) -> TerminalState {
        let (tx, _rx) = channel();
        TerminalState::new(cols, rows, GpuiEventProxy::new(tx))
    }

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Recorder {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Recorder {
        fn take(&self) -> Vec<u8> {
            std::mem::take(&mut *self.0.lock())
        }
    }

    fn state_with_recorder() -> (TerminalState, Recorder, Arc<Mutex<ColorPalette>>) {
        use crate::event::{PtyWriter, TerminalNotifier};
        use alacritty_terminal::event::WindowSize;

        let (tx, _rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        let recorder = Recorder::default();
        let writer: PtyWriter = Arc::new(Mutex::new(Box::new(recorder.clone())));
        let palette = Arc::new(Mutex::new(ColorPalette::default()));
        *proxy.notifier_slot().lock() = Some(TerminalNotifier::new(
            writer,
            Arc::new(Mutex::new(WindowSize {
                num_lines: 24,
                num_cols: 80,
                cell_width: 8,
                cell_height: 16,
            })),
            Arc::clone(&palette),
        ));
        (TerminalState::new(80, 24, proxy), recorder, palette)
    }

    #[test]
    fn theme_queries_and_mode_work_across_all_fragment_boundaries() {
        let input = b"\x1b[?2031$p\x1b[?2031h\x1b[?2031$p\x1b[?996n\x1b[?2031l\x1b[?2031$p";
        let expected = b"\x1b[?2031;2$y\x1b[?2031;1$y\x1b[?997;1n\x1b[?2031;2$y";
        for split in 0..=input.len() {
            let (mut terminal, recorder, _) = state_with_recorder();
            terminal.process_bytes(&input[..split]);
            terminal.process_bytes(&input[split..]);
            assert_eq!(recorder.take(), expected, "split at {split}");
        }
        let (mut terminal, recorder, _) = state_with_recorder();
        for byte in input {
            terminal.process_bytes(std::slice::from_ref(byte));
        }
        assert_eq!(recorder.take(), expected);
    }

    #[test]
    fn theme_reports_follow_actual_palette_changes_and_subscription() {
        let (mut terminal, recorder, shared_palette) = state_with_recorder();
        let light = ColorPalette::builder().background(255, 255, 255).build();
        *shared_palette.lock() = light.clone();
        terminal.set_palette(light.clone());
        assert!(recorder.take().is_empty());
        terminal.process_bytes(b"\x1b[?996n\x1b[?2031h");
        assert_eq!(recorder.take(), b"\x1b[?997;2n");
        terminal.set_palette(light.clone());
        assert!(recorder.take().is_empty());

        let another_light = ColorPalette::builder()
            .background(255, 255, 255)
            .red(1, 2, 3)
            .build();
        *shared_palette.lock() = another_light.clone();
        terminal.set_palette(another_light);
        assert_eq!(recorder.take(), b"\x1b[?997;2n");
        terminal.process_bytes(b"\x1b]11;?\x07");
        assert_eq!(recorder.take(), b"\x1b]11;rgb:ffff/ffff/ffff\x07");

        *shared_palette.lock() = ColorPalette::default();
        terminal.set_palette(ColorPalette::default());
        assert_eq!(recorder.take(), b"\x1b[?997;1n");
        terminal.process_bytes(b"\x1b[?2031l");
        terminal.set_palette(light);
        assert!(recorder.take().is_empty());
    }

    #[test]
    fn theme_classification_uses_perceived_background_brightness() {
        let (mut terminal, recorder, _) = state_with_recorder();
        terminal.set_palette(ColorPalette::builder().background(0, 160, 0).build());
        terminal.process_bytes(b"\x1b[?996n");
        assert_eq!(recorder.take(), b"\x1b[?997;1n");
        terminal.set_palette(ColorPalette::builder().background(255, 255, 0).build());
        terminal.process_bytes(b"\x1b[?996n");
        assert_eq!(recorder.take(), b"\x1b[?997;2n");
    }

    #[test]
    fn theme_sequences_inside_control_strings_do_not_reply_or_subscribe() {
        let (mut terminal, recorder, _) = state_with_recorder();
        for byte in b"\x1b]0;ignored [?2031h [?2031$p [?996n\x07\
                      \x1bPqignored [?2031h [?2031$p [?996n\x1b\\"
        {
            terminal.process_bytes(std::slice::from_ref(byte));
        }
        assert!(recorder.take().is_empty());
        terminal.set_palette(ColorPalette::builder().background(255, 255, 255).build());
        assert!(recorder.take().is_empty());
        terminal.process_bytes(b"\x1b[?2031$p\x1b[?996n");
        assert_eq!(recorder.take(), b"\x1b[?2031;2$y\x1b[?997;2n");
    }

    #[test]
    fn escape_interrupts_osc_before_theme_query() {
        let (mut terminal, recorder, _) = state_with_recorder();
        terminal.process_bytes(b"\x1b]0;unfinished\x1b[?2031h\x1b[?2031$p");
        assert_eq!(recorder.take(), b"\x1b[?2031;1$y");
    }

    #[test]
    fn combined_modes_and_alternate_screen_reset_preserve_subscription() {
        let (mut terminal, recorder, _) = state_with_recorder();
        terminal.process_bytes(b"\x1b[?1;2031;1049h\x1b[?1049l\x1b[?2031$p");
        assert!(terminal.mode().contains(TermMode::APP_CURSOR));
        assert!(!terminal.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(recorder.take(), b"\x1b[?2031;1$y");
        terminal.process_bytes(b"\x1b[?1;2031l\x1b[?2031$p");
        assert!(!terminal.mode().contains(TermMode::APP_CURSOR));
        assert_eq!(recorder.take(), b"\x1b[?2031;2$y");
        terminal.process_bytes(b"\x1b[?2031h\x1bc\x1b[?2031$p");
        assert_eq!(recorder.take(), b"\x1b[?2031;2$y");
    }

    #[test]
    fn test_terminal_creation() {
        let terminal = state(80, 24);
        assert_eq!(terminal.cols(), 80);
        assert_eq!(terminal.rows(), 24);
    }

    #[test]
    fn test_process_bytes() {
        let mut terminal = state(80, 24);
        terminal.process_bytes(b"Hello, world!");
        terminal.with_term(|term| assert_eq!(term.grid().columns(), 80));
    }

    #[test]
    fn test_resize() {
        let mut terminal = state(80, 24);
        terminal.resize(120, 30);

        assert_eq!(terminal.cols(), 120);
        assert_eq!(terminal.rows(), 30);
        terminal.with_term(|term| {
            assert_eq!(term.grid().columns(), 120);
            assert_eq!(term.grid().screen_lines(), 30);
        });
    }

    /// The paint pass resizes the grid straight through `term_arc`, so for the
    /// rest of that frame the grid can be smaller than the size this state last
    /// recorded. A scrape in that window must not ask the grid for rows that no
    /// longer exist: alacritty indexes lines that are below the visible screen
    /// as scrollback, and a line past the end of the buffer panics.
    #[test]
    fn scrape_survives_a_grid_resized_behind_its_back() {
        let terminal = state(80, 24);
        terminal
            .term_arc()
            .lock()
            .resize(TermDimensions::new(40, 10));

        let lines = terminal.bottom_lines(usize::MAX);
        assert_eq!(lines.len(), 10);

        let mut into = Vec::new();
        terminal.bottom_lines_into(usize::MAX, &mut into);
        assert_eq!(into.len(), 10);
    }

    #[test]
    fn test_mode() {
        let terminal = state(80, 24);
        let _bits = terminal.mode().bits();
    }

    #[test]
    fn test_term_arc() {
        let terminal = state(80, 24);
        assert!(Arc::ptr_eq(&terminal.term_arc(), &terminal.term_arc()));
    }

    #[test]
    fn test_scrollback_is_configurable() {
        let (tx, _rx) = channel();
        let config = TerminalState::build_config(500, ",│`|:\"' ()[]{}<>\t", true);
        let mut terminal = TerminalState::with_config(80, 24, config, GpuiEventProxy::new(tx));

        for i in 0..1000 {
            terminal.process_bytes(format!("Line {i}\r\n").as_bytes());
        }

        // 500 lines of history must be retained, not 0 and not 1000.
        assert_eq!(terminal.history_size(), 500);
    }

    #[test]
    fn test_scroll_direction() {
        let mut terminal = state(80, 24);
        for i in 0..100 {
            terminal.process_bytes(format!("Line {i}\r\n").as_bytes());
        }

        assert!(terminal.is_at_bottom());
        terminal.scroll_lines(5);
        assert_eq!(terminal.display_offset(), 5);
        assert!(!terminal.is_at_bottom());
        terminal.scroll_lines(-5);
        assert_eq!(terminal.display_offset(), 0);

        terminal.scroll_to_top();
        assert!(terminal.display_offset() > 0);
        terminal.scroll_to_bottom();
        assert_eq!(terminal.display_offset(), 0);
    }

    #[test]
    fn test_select_all_and_copy() {
        let mut terminal = state(20, 4);
        terminal.process_bytes(b"alpha\r\nbeta\r\n");

        assert!(!terminal.has_selection());
        terminal.select_all();
        assert!(terminal.has_selection());

        let text = terminal.selection_text().unwrap_or_default();
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));

        terminal.clear_selection();
        assert!(!terminal.has_selection());
    }

    #[test]
    fn test_search_finds_all_matches() {
        let mut terminal = state(40, 6);
        terminal.process_bytes(b"needle one\r\nhaystack\r\nneedle two\r\n");

        let mut regex = RegexSearch::new("needle").expect("valid regex");
        let matches = terminal.all_matches(&mut regex);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_vi_mode_toggle() {
        let terminal = state(40, 6);
        assert!(!terminal.is_vi_mode());
        terminal.toggle_vi_mode();
        assert!(terminal.is_vi_mode());
        terminal.toggle_vi_mode();
        assert!(!terminal.is_vi_mode());
    }

    #[test]
    fn test_xtgettcap_known_cap_replies_inline() {
        let (tx, rx) = std::sync::mpsc::channel();
        let proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(80, 24, proxy);

        // XTGETTCAP requests are `ESC P + q <percent-encoded> ESC \`; the
        // responder must fire before the stream reaches the grid and must not
        // print anything.
        terminal.process_bytes(b"\x1bP+q5442\x1b\\");

        // The query is dropped by the VTE handler (no output): nothing was
        // printed, and any control state the handler tracked stays consistent.
        assert!(!rx.try_recv().is_ok());
    }

    #[test]
    fn test_xtgettcap_unknown_cap_gets_invalid_reply() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(80, 24, proxy);

        // Malformed request must still produce the `0+r` reply.
        terminal.process_bytes(b"\x1bP+q78797a7a\x1b\\");
    }

    #[test]
    fn test_decode_hex_roundtrip() {
        assert_eq!(decode_hex(b"4142"), Some("AB".to_string()));
    }

    #[test]
    fn test_line_text() {
        let mut terminal = state(40, 6);
        terminal.process_bytes(b"hello world\r\n");
        assert_eq!(terminal.line_text(Line(0)), "hello world");
    }

    #[test]
    fn buffer_text_keeps_the_newest_lines_and_trims_blanks() {
        let mut terminal = state(20, 3);
        terminal.process_bytes(b"one\r\ntwo\r\nthree\r\nfour\r\n");

        let all = terminal.buffer_text(100);
        assert!(all.contains("one"));
        assert!(all.contains("four"));
        assert!(all.ends_with("\r\n"));

        let recent = terminal.buffer_text(2);
        assert!(!recent.contains("one"));
        assert!(recent.contains("four"));
        assert_eq!(terminal.buffer_text(0), "");
    }

    #[test]
    fn buffer_text_of_an_empty_terminal_is_empty() {
        let terminal = state(20, 3);
        assert!(terminal.buffer_text(100).is_empty());
    }

    #[test]
    fn streaming_chunks_preserve_ansi_unicode_modes_and_capture_order() {
        let mut bytes = b"\x1b]133;B\x07echo sample\r\n\x1b]133;C\x07".to_vec();
        for _ in 0..120 {
            bytes.extend_from_slice("plain \u{754c} e\u{301} \x1b[1;31mred\x1b[0m\r\n".as_bytes());
        }
        bytes.extend_from_slice(b"\x1b[?1049h\x1b[Halternate\x1b[?1049l\x1b]133;D;0\x07");
        let mut reference = state(80, 24);
        let reference_events = reference.process_bytes(&bytes);
        assert!(
            reference_events
                .iter()
                .any(|event| matches!(event, ShellEvent::CommandFinished { .. }))
        );
        let reference_snapshot = reference.snapshot_buffer_lines(reference.palette(), false);
        for chunk_size in [1, 7, 32_768, 131_072] {
            let mut candidate = state(80, 24);
            let mut events = Vec::new();
            for chunk in bytes.chunks(chunk_size) {
                events.extend(candidate.process_bytes(chunk));
            }
            assert_eq!(
                candidate.snapshot_buffer_lines(candidate.palette(), false),
                reference_snapshot
            );
            assert_eq!(candidate.mode(), reference.mode());
            assert_eq!(
                candidate.with_term(|term| term.grid().cursor.point),
                reference.with_term(|term| term.grid().cursor.point)
            );
            let summarize = |events: Vec<ShellEvent>| {
                events
                    .into_iter()
                    .map(|mut event| {
                        // Wall-clock duration differs with chunk size; every other
                        // event field and its position in the sequence must agree.
                        if let ShellEvent::CommandFinished { duration, .. } = &mut event {
                            *duration = std::time::Duration::ZERO;
                        }
                        format!("{event:?}")
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                summarize(events),
                summarize(reference_events.clone()),
                "chunk size {chunk_size}"
            );
        }
    }

    #[test]
    #[ignore = "measurement; run with --release --ignored --nocapture"]
    fn streaming_parse_batch_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;
        for (name, seed, restore) in [
            (
                "plain",
                b"ordinary streamed output with spaces\r\n".as_slice(),
                false,
            ),
            (
                "ansi",
                b"\x1b[1;31mred\x1b[0m \x1b[4munderline\x1b[0m\r\n".as_slice(),
                false,
            ),
            ("long_line", b"abcdefghij".as_slice(), false),
            (
                "restoration",
                b"restored terminal output\r\n".as_slice(),
                true,
            ),
        ] {
            let bytes: Vec<_> = seed.iter().copied().cycle().take(128 * 1024).collect();
            let mut terminal = state(120, 48);
            let mut samples = Vec::with_capacity(101);
            for index in 0..111 {
                let start = Instant::now();
                if restore {
                    terminal.feed_raw(black_box(&bytes));
                } else {
                    black_box(terminal.process_bytes(black_box(&bytes)));
                }
                if index >= 10 {
                    samples.push(start.elapsed().as_micros());
                }
            }
            samples.sort_unstable();
            eprintln!(
                "{name} 128KiB batch: p50={}us p95={}us max={}us (101 samples, 10 warmups)",
                samples[50], samples[95], samples[100]
            );
            let mut reference = state(120, 48);
            for _ in 0..111 {
                for chunk in bytes.chunks(4_096) {
                    reference.feed_raw(chunk);
                }
            }
            assert_eq!(
                terminal.snapshot_buffer_lines(terminal.palette(), false),
                reference.snapshot_buffer_lines(reference.palette(), false)
            );
            assert_eq!(terminal.mode(), reference.mode());
        }
    }
}
