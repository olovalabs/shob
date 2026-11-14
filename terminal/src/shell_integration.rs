//! Shell integration: OSC 133 semantic prompt markers, cwd/metadata, output
//! capture and the re-parser that turns captured output into immutable blocks.
//!
//! The scanner observes the raw PTY byte stream *before* the VTE parser. It
//! never rewrites or removes bytes. Markers are reported with the offset of
//! their terminator in the current chunk so [`crate::terminal::TerminalState`]
//! can advance the parser exactly up to each marker boundary, which is what
//! makes cursor positions at `B`/`C`/`D` accurate.

use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use gpui::Hsla;

use crate::colors::ColorPalette;
use crate::event::GpuiEventProxy;
use crate::render::TerminalRenderer;
use crate::terminal::TerminalState;

/// Largest OSC payload we are willing to buffer while looking for a marker.
const MAX_OSC_BYTES: usize = 8 * 1024;

/// Largest captured output retained per block. Beyond this the block is marked
/// truncated and further bytes are dropped from the capture (the live grid
/// still receives them).
pub const MAX_BLOCK_BYTES: usize = 4 * 1024 * 1024;

/// Scrollback used by the throw-away terminal that re-parses captured output.
const MAX_BLOCK_LINES: usize = 20_000;

/// A styled run inside a captured block line.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockRun {
    pub text: String,
    pub fg: Hsla,
    pub bg: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub wavy: bool,
}

/// One logical (unwrapped) line of captured output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockLine {
    pub runs: Vec<BlockRun>,
    /// Display cells occupied by the line after trailing-space trimming.
    pub cells: usize,
}

/// Immutable output capture for one block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockSnapshot {
    pub lines: Vec<BlockLine>,
    pub truncated: bool,
}

impl BlockSnapshot {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Plain text of the block, one logical line per row.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            for run in &line.runs {
                out.push_str(&run.text);
            }
        }
        out
    }
}

/// Shell integration events emitted by the terminal view on the UI thread.
#[derive(Clone, Debug)]
pub enum ShellEvent {
    /// `OSC 133;A` — a prompt is about to be drawn.
    PromptStart,
    /// `OSC 133;B` — the prompt ended and command input starts here.
    PromptEnd,
    /// `OSC 133;C` — the command was accepted and output starts here.
    CommandStarted {
        command_line: Option<String>,
        cwd: Option<String>,
        cols: usize,
        rows: usize,
    },
    /// `OSC 133;D[;exit]` — the command finished.
    CommandFinished {
        exit_code: Option<i32>,
        duration: Duration,
        raw: Vec<u8>,
        truncated: bool,
        full_screen: bool,
        cols: usize,
        rows: usize,
    },
    /// `OSC 7` or `OSC 7502;cwd=…`.
    CwdChanged(String),
    /// `OSC 7502;<key>=<value>:…`.
    Metadata(Vec<(String, String)>),
}

/// Marker kinds reported by the scanner. Owned payloads are only allocated for
/// the rare cwd/metadata markers.
#[derive(Debug)]
pub(crate) enum MarkerKind {
    PromptStart,
    PromptEnd,
    /// `133;C`, optionally carrying fish's `cmdline_url=…` payload.
    OutputStart(Option<String>),
    OutputEnd(Option<i32>),
    Cwd(String),
    Metadata(Vec<(String, String)>),
}

#[derive(Debug)]
pub(crate) struct MarkerHit {
    /// Offset in the current chunk just past the marker terminator.
    pub end: usize,
    pub kind: MarkerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Ground,
    Escape,
    Osc,
    OscEscape,
    Ignore,
    IgnoreEscape,
}

#[derive(Debug)]
struct MarkerScanner {
    state: ScanState,
    body: Vec<u8>,
    /// Only accept OSC 133 markers stamped with this token. Empty accepts all
    /// (unit tests and terminals without app integration).
    token: String,
}

impl Default for MarkerScanner {
    fn default() -> Self {
        Self {
            state: ScanState::Ground,
            body: Vec::with_capacity(128),
            token: String::new(),
        }
    }
}

impl MarkerScanner {
    /// Feed a chunk and return every marker whose terminator is in it.
    fn scan(&mut self, bytes: &[u8]) -> Vec<MarkerHit> {
        let mut hits = Vec::new();
        for (index, &byte) in bytes.iter().enumerate() {
            match self.state {
                ScanState::Ground => match byte {
                    0x1b => self.state = ScanState::Escape,
                    0x9d => self.begin_osc(),
                    _ => {}
                },
                ScanState::Escape => match byte {
                    b']' | 0x9d => self.begin_osc(),
                    b'c' => {
                        self.reset();
                        self.state = ScanState::Ground;
                    }
                    0x1b => self.state = ScanState::Escape,
                    _ => self.state = ScanState::Ground,
                },
                ScanState::Osc => match byte {
                    0x07 | 0x9c => {
                        if let Some(kind) = self.finish_osc() {
                            hits.push(MarkerHit {
                                end: index + 1,
                                kind,
                            });
                        }
                    }
                    0x1b => self.state = ScanState::OscEscape,
                    _ => self.append(byte),
                },
                ScanState::OscEscape => match byte {
                    b'\\' => {
                        if let Some(kind) = self.finish_osc() {
                            hits.push(MarkerHit {
                                end: index + 1,
                                kind,
                            });
                        }
                    }
                    b']' | 0x9d => self.begin_osc(),
                    0x1b => self.state = ScanState::OscEscape,
                    _ => self.state = ScanState::Ground,
                },
                ScanState::Ignore => match byte {
                    0x07 | 0x9c => self.state = ScanState::Ground,
                    0x1b => self.state = ScanState::IgnoreEscape,
                    _ => {}
                },
                ScanState::IgnoreEscape => match byte {
                    b'\\' => self.state = ScanState::Ground,
                    0x1b => self.state = ScanState::IgnoreEscape,
                    _ => self.state = ScanState::Ignore,
                },
            }
        }
        hits
    }

    fn begin_osc(&mut self) {
        self.body.clear();
        self.state = ScanState::Osc;
    }

    fn append(&mut self, byte: u8) {
        if self.body.len() >= MAX_OSC_BYTES {
            self.body.clear();
            self.state = ScanState::Ignore;
        } else {
            self.body.push(byte);
        }
    }

    fn finish_osc(&mut self) -> Option<MarkerKind> {
        let kind = parse_osc_body(&self.body, &self.token);
        self.body.clear();
        self.state = ScanState::Ground;
        kind
    }

    fn reset(&mut self) {
        self.body.clear();
        self.state = ScanState::Ground;
    }
}

#[derive(Debug)]
struct Capture {
    raw: Vec<u8>,
    cols: usize,
    rows: usize,
    started: Instant,
    truncated: bool,
    full_screen: bool,
}

/// Stateful shell-integration tracker owned by `TerminalState`.
#[derive(Debug, Default)]
pub(crate) struct ShellIntegrationScanner {
    markers: MarkerScanner,
    capture: Option<Capture>,
    cwd: Option<String>,
    /// Cursor at `OSC 133;B` plus the history size then, so `C` can map it
    /// back even when the command echo scrolled the grid.
    command_start: Option<(Point, usize)>,
    last_alt_screen: bool,
    capture_disabled: bool,
}

impl ShellIntegrationScanner {
    pub(crate) fn set_token(&mut self, token: String) {
        self.markers.token = token;
    }

    pub(crate) fn set_capture_output(&mut self, enabled: bool) {
        self.capture_disabled = !enabled;
        if !enabled {
            if let Some(capture) = self.capture.as_mut() {
                capture.raw = Vec::new();
            }
        }
    }

    pub(crate) fn scan(&mut self, bytes: &[u8]) -> Vec<MarkerHit> {
        self.markers.scan(bytes)
    }

    /// Append parser-consumed bytes to the active capture.
    pub(crate) fn capture_bytes(&mut self, bytes: &[u8]) {
        if self.capture_disabled {
            return;
        }
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        if capture.truncated {
            return;
        }
        if bytes_enter_alt_screen(bytes) {
            capture.full_screen = true;
        }
        let remaining = MAX_BLOCK_BYTES.saturating_sub(capture.raw.len());
        if bytes.len() > remaining {
            capture.raw.extend_from_slice(&bytes[..remaining]);
            capture.truncated = true;
        } else {
            capture.raw.extend_from_slice(bytes);
        }
    }

    /// Track alternate-screen transitions while a block is being captured.
    pub(crate) fn note_mode(&mut self, alt_screen: bool) {
        if alt_screen && !self.last_alt_screen {
            if let Some(capture) = self.capture.as_mut() {
                capture.full_screen = true;
            }
        }
        self.last_alt_screen = alt_screen;
    }

    /// Apply a completed marker after the parser consumed every byte up to
    /// `hit.end`.
    pub(crate) fn apply(
        &mut self,
        kind: MarkerKind,
        term: &Term<GpuiEventProxy>,
        now: Instant,
        out: &mut Vec<ShellEvent>,
    ) {
        // Full-screen applications (including agent TUIs) sometimes emit their
        // own OSC 133 sequences. Those are not shell prompts; treating them as
        // blocks produces a pile of empty, bogus cards. Keep only cwd metadata
        // while the alternate screen owns the terminal.
        if term.mode().contains(TermMode::ALT_SCREEN)
            && !matches!(kind, MarkerKind::Cwd(_) | MarkerKind::Metadata(_))
        {
            return;
        }

        match kind {
            MarkerKind::PromptStart => {
                self.finish_capture(None, now, out);
                self.command_start = None;
                out.push(ShellEvent::PromptStart);
            }
            MarkerKind::PromptEnd => {
                self.command_start = Some((term.grid().cursor.point, term.grid().history_size()));
                out.push(ShellEvent::PromptEnd);
            }
            MarkerKind::OutputStart(marker_command) => {
                let command_line = marker_command.or_else(|| self.command_line(term));
                let cols = term.grid().columns();
                let rows = term.grid().screen_lines();
                self.capture = Some(Capture {
                    raw: Vec::new(),
                    cols,
                    rows,
                    started: now,
                    truncated: false,
                    full_screen: term.mode().contains(TermMode::ALT_SCREEN),
                });
                out.push(ShellEvent::CommandStarted {
                    command_line,
                    cwd: self.cwd.clone(),
                    cols,
                    rows,
                });
            }
            MarkerKind::OutputEnd(exit_code) => {
                self.finish_capture(exit_code, now, out);
                self.command_start = None;
            }
            MarkerKind::Cwd(path) => {
                self.cwd = Some(path.clone());
                out.push(ShellEvent::CwdChanged(path));
            }
            MarkerKind::Metadata(fields) => {
                if let Some(path) = fields
                    .iter()
                    .find(|(key, _)| key == "cwd")
                    .map(|(_, value)| value.clone())
                {
                    self.cwd = Some(path.clone());
                    out.push(ShellEvent::CwdChanged(path));
                }
                out.push(ShellEvent::Metadata(fields));
            }
        }
    }

    /// Cwd reported by the shell, if it has told us.
    pub(crate) fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    fn finish_capture(&mut self, exit_code: Option<i32>, now: Instant, out: &mut Vec<ShellEvent>) {
        let Some(capture) = self.capture.take() else {
            return;
        };
        out.push(ShellEvent::CommandFinished {
            exit_code,
            duration: now.saturating_duration_since(capture.started),
            raw: capture.raw,
            truncated: capture.truncated,
            full_screen: capture.full_screen,
            cols: capture.cols,
            rows: capture.rows,
        });
    }

    fn command_line(&self, term: &Term<GpuiEventProxy>) -> Option<String> {
        let (start_point, start_history) = self.command_start?;
        let grid = term.grid();
        let history_now = grid.history_size();
        let pushed = history_now.saturating_sub(start_history) as i32;

        // Map the `B` cursor into current buffer coordinates. The echoed
        // command may have pushed the prompt line into scrollback.
        let start_line = start_point.line.0 - pushed;
        let start_col = start_point.column.0;
        let end_line = grid.cursor.point.line.0;
        if end_line < start_line {
            return None;
        }

        let mut text = String::new();
        for line in start_line..=end_line {
            let first_col = if line == start_line { start_col } else { 0 };
            if first_col >= grid.columns() {
                continue;
            }
            let mut line_text = term.bounds_to_string(
                Point::new(Line(line), Column(first_col)),
                Point::new(Line(line), term.last_column()),
            );
            while line_text.ends_with(' ') {
                line_text.pop();
            }
            text.push_str(&line_text);
        }

        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    }
}

/// Detect alternate-screen entry directly in the byte stream. A command can
/// enter and leave the alt screen inside one read, in which case polling
/// `TermMode` after the chunk only sees the final (normal) state.
fn bytes_enter_alt_screen(bytes: &[u8]) -> bool {
    const ENTERS: [&[u8]; 3] = [b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"];
    ENTERS.iter().any(|needle| {
        bytes.len() >= needle.len() && bytes.windows(needle.len()).any(|window| window == *needle)
    })
}

fn parse_osc_body(body: &[u8], expected_token: &str) -> Option<MarkerKind> {
    if let Some(rest) = body.strip_prefix(b"133;") {
        let kind = rest.first()?;
        let params = rest.get(1..).unwrap_or_default();
        if !expected_token.is_empty()
            && !std::str::from_utf8(params)
                .ok()?
                .split(';')
                .any(|param| param.strip_prefix("KATHAMO=") == Some(expected_token))
        {
            return None;
        }
        return match kind {
            b'A' => Some(MarkerKind::PromptStart),
            b'B' => Some(MarkerKind::PromptEnd),
            b'C' => Some(MarkerKind::OutputStart(parse_cmdline_url(params))),
            b'D' => Some(MarkerKind::OutputEnd(parse_exit_code(params))),
            _ => None,
        };
    }

    if let Some(rest) = body.strip_prefix(b"7;") {
        return parse_osc7(rest).map(MarkerKind::Cwd);
    }

    if let Some(rest) = body.strip_prefix(b"7502;") {
        return parse_metadata(rest).map(MarkerKind::Metadata);
    }

    None
}

/// fish emits `133;C;cmdline_url=<percent-encoded command>`, which is more
/// reliable than reconstructing the command from the screen.
fn parse_cmdline_url(params: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(params).ok()?;
    for param in text.split(';') {
        if let Some(value) = param.strip_prefix("cmdline_url=") {
            let decoded = percent_decode(value);
            let decoded = decoded.trim();
            if !decoded.is_empty() {
                return Some(decoded.to_string());
            }
        }
    }
    None
}

fn parse_exit_code(rest: &[u8]) -> Option<i32> {
    let rest = rest.strip_prefix(b";")?;
    let text = std::str::from_utf8(rest).ok()?;
    text.split(';')
        .next()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|code| (-256..=255).contains(code))
}

fn parse_osc7(rest: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(rest).ok()?.trim();
    let path = if let Some(after_scheme) = text.strip_prefix("file://") {
        // authority is everything before the first `/`; the path starts there.
        let slash = after_scheme.find('/')?;
        &after_scheme[slash..]
    } else {
        text
    };
    if path.is_empty() {
        return None;
    }
    Some(percent_decode(path))
}

fn parse_metadata(rest: &[u8]) -> Option<Vec<(String, String)>> {
    let text = std::str::from_utf8(rest).ok()?.trim();
    if text.is_empty() || text.len() > 2048 {
        return None;
    }
    let mut fields = Vec::new();
    for pair in text.split(':').take(16) {
        let Some((key, value)) = pair.split_once('=') else {
            return None;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || key.len() > 32 || value.len() > 1024 {
            return None;
        }
        if !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.+".contains(&b))
        {
            return None;
        }
        fields.push((key.to_string(), percent_decode(value)));
    }
    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = (bytes[index + 1] as char).to_digit(16);
            let lo = (bytes[index + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi << 4) | lo) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Re-parse captured output at `cols`/`rows` and snapshot the styled lines.
///
/// This runs in a throw-away terminal, so it is safe to call from any thread
/// (and is reused verbatim for reflow when a pane changes width).
pub fn render_output_lines(
    raw: &[u8],
    cols: usize,
    rows: usize,
    palette: &ColorPalette,
) -> BlockSnapshot {
    let (tx, _rx) = std::sync::mpsc::channel();
    let config = TerminalState::build_config(MAX_BLOCK_LINES, "", false);
    let mut state =
        TerminalState::with_config(cols.max(2), rows.max(1), config, GpuiEventProxy::new(tx));
    state.set_palette(palette.clone());
    state.feed_raw(raw);
    state.snapshot_buffer_lines(palette, false)
}

/// Snapshot the buffer lines of a terminal, joining hard-wrapped lines.
pub(crate) fn snapshot_term(
    term: &Term<GpuiEventProxy>,
    palette: &ColorPalette,
    truncated: bool,
) -> BlockSnapshot {
    let grid = term.grid();
    let colors = term.colors();
    let history = grid.history_size() as i32;
    let cursor_line = grid.cursor.point.line.0;
    let cols = grid.columns();
    if cols == 0 {
        return BlockSnapshot::default();
    }
    let last_col = Column(cols.saturating_sub(1));
    let renderer = TerminalRenderer::new(String::new(), gpui::px(13.5), 1.2, palette.clone());

    let mut physical = Vec::with_capacity((cursor_line + history + 1).max(0) as usize);
    for line_index in -history..=cursor_line {
        let line = Line(line_index);
        let wrapped = grid[Point::new(line, last_col)]
            .flags
            .contains(Flags::WRAPLINE);
        let cells = (0..cols).map(|col| (col, grid[Point::new(line, Column(col))].clone()));
        let block_line = renderer.snapshot_line(line_index.max(0) as usize, cells, colors);
        physical.push((block_line, wrapped));
    }

    // Join hard-wrapped physical lines into logical lines.
    let mut lines: Vec<BlockLine> = Vec::new();
    let mut current: Option<BlockLine> = None;
    for (line, wrapped) in physical {
        match current.as_mut() {
            Some(cur) => {
                cur.runs.extend(line.runs);
                cur.cells = cur.cells.saturating_add(line.cells);
            }
            None => current = Some(line),
        }
        if !wrapped {
            if let Some(done) = current.take() {
                lines.push(done);
            }
        }
    }
    if let Some(done) = current.take() {
        lines.push(done);
    }

    while lines.last().is_some_and(|line| line.runs.is_empty()) {
        lines.pop();
    }

    BlockSnapshot { lines, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::GpuiEventProxy;
    use std::sync::mpsc::channel;

    fn state(cols: usize, rows: usize) -> TerminalState {
        let (tx, _rx) = channel();
        TerminalState::new(cols, rows, GpuiEventProxy::new(tx))
    }

    fn kinds(events: &[ShellEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                ShellEvent::PromptStart => "A",
                ShellEvent::PromptEnd => "B",
                ShellEvent::CommandStarted { .. } => "C",
                ShellEvent::CommandFinished { .. } => "D",
                ShellEvent::CwdChanged(_) => "cwd",
                ShellEvent::Metadata(_) => "meta",
            })
            .collect()
    }

    #[test]
    fn parses_markers_with_all_terminators() {
        for (label, bytes) in [
            ("bel", b"\x1b]133;A\x07".as_slice()),
            ("st", b"\x1b]133;A\x1b\\".as_slice()),
            ("c1", b"\x9d133;A\x9c".as_slice()),
        ] {
            let mut scanner = MarkerScanner::default();
            let hits = scanner.scan(bytes);
            assert_eq!(hits.len(), 1, "{label}");
            assert!(matches!(hits[0].kind, MarkerKind::PromptStart), "{label}");
            assert_eq!(hits[0].end, bytes.len(), "{label}");
        }
    }

    #[test]
    fn marker_split_across_chunks_is_reported_once() {
        let bytes = b"\x1b]133;C\x07";
        let mut scanner = MarkerScanner::default();
        let mut hits = Vec::new();
        for byte in bytes {
            hits.extend(scanner.scan(&[*byte]));
        }
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0].kind, MarkerKind::OutputStart(_)));
    }

    #[test]
    fn exit_code_parsing() {
        let mut scanner = MarkerScanner::default();
        let hits = scanner.scan(b"\x1b]133;D;7\x07");
        assert!(matches!(hits[0].kind, MarkerKind::OutputEnd(Some(7))));

        let hits = scanner.scan(b"\x1b]133;D\x07");
        assert!(matches!(hits[0].kind, MarkerKind::OutputEnd(None)));

        let hits = scanner.scan(b"\x1b]133;D;9999\x07");
        assert!(matches!(hits[0].kind, MarkerKind::OutputEnd(None)));
    }

    #[test]
    fn osc7_and_metadata() {
        let mut scanner = MarkerScanner::default();
        let hits = scanner.scan(b"\x1b]7;file://host/tmp/a%20b\x07");
        match &hits[0].kind {
            MarkerKind::Cwd(path) => assert_eq!(path, "/tmp/a b"),
            other => panic!("unexpected {other:?}"),
        }

        let hits = scanner.scan(b"\x1b]7502;cwd=/tmp/x:host=dev\x07");
        match &hits[0].kind {
            MarkerKind::Metadata(fields) => {
                assert!(fields.contains(&("cwd".into(), "/tmp/x".into())));
                assert!(fields.contains(&("host".into(), "dev".into())));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_and_oversized_osc_are_ignored() {
        let mut scanner = MarkerScanner::default();
        assert!(scanner.scan(b"\x1b]999;whatever\x07").is_empty());
        let mut payload = vec![b'x'; MAX_OSC_BYTES + 8];
        payload.extend_from_slice(b"\x07\x1b]133;A\x07");
        let hits = scanner.scan(&payload);
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0].kind, MarkerKind::PromptStart));
    }

    #[test]
    fn terminal_emits_prompt_and_command_events() {
        let mut terminal = state(20, 4);
        let events = terminal.process_bytes(b"\x1b]133;A\x07");
        assert_eq!(kinds(&events), vec!["A"]);

        let events = terminal.process_bytes(b"\x1b]133;B\x07");
        assert_eq!(kinds(&events), vec!["B"]);

        let events = terminal.process_bytes(b"\x1b]133;C\x07hello\r\n");
        assert!(matches!(events[0], ShellEvent::CommandStarted { .. }));

        let events = terminal.process_bytes(b"\x1b]133;D;0\x07");
        match &events[0] {
            ShellEvent::CommandFinished { exit_code, raw, .. } => {
                assert_eq!(*exit_code, Some(0));
                assert!(String::from_utf8_lossy(raw).contains("hello"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn capture_can_be_disabled_without_losing_command_status_or_live_output() {
        let mut terminal = state(20, 4);
        terminal.set_capture_output(false);
        terminal.process_bytes(b"\x1b]133;C\x07hello\r\n");
        let events = terminal.process_bytes(b"\x1b]133;D;7\x07");
        match &events[0] {
            ShellEvent::CommandFinished { exit_code, raw, .. } => {
                assert_eq!(*exit_code, Some(7));
                assert!(raw.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
        let snapshot = terminal.snapshot_buffer_lines(&ColorPalette::default(), false);
        let text: String = snapshot
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .map(|run| run.text.as_str())
            .collect();
        assert!(text.contains("hello"));
        terminal.set_capture_output(true);
        terminal.process_bytes(b"\x1b]133;C\x07captured\r\n");
        let events = terminal.process_bytes(b"\x1b]133;D;0\x07");
        assert!(
            matches!(&events[0], ShellEvent::CommandFinished { raw, .. } if raw.windows(8).any(|bytes| bytes == b"captured"))
        );
    }

    #[test]
    fn dangling_command_is_finalized_on_prompt_start() {
        let mut terminal = state(20, 4);
        terminal.process_bytes(b"\x1b]133;C\x07");
        let events = terminal.process_bytes(b"\x1b]133;A\x07");
        assert_eq!(kinds(&events), vec!["D", "A"]);
        match &events[0] {
            ShellEvent::CommandFinished { exit_code, .. } => assert_eq!(*exit_code, None),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn alt_screen_during_a_command_marks_the_block_full_screen() {
        let mut terminal = state(40, 10);
        terminal.process_bytes(b"\x1b]133;C\x07");
        // Entering the alternate screen while capturing (vim/htop/less), then
        // leaving it before the shell reports the command's exit.
        terminal.process_bytes(b"\x1b[?1049hALT SCREEN\x1b[?1049l");
        let events = terminal.process_bytes(b"\x1b]133;D;0\x07");
        match &events[0] {
            ShellEvent::CommandFinished { full_screen, .. } => {
                assert!(*full_screen, "alt-screen command must not be replayed");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn fish_cmdline_url_is_used_as_the_command() {
        let mut terminal = state(60, 10);
        terminal.process_bytes(b"\x1b]133;A\x07");
        let events = terminal.process_bytes(b"\x1b]133;C;cmdline_url=echo%20hello%20world\x07");
        match &events[0] {
            ShellEvent::CommandStarted { command_line, .. } => {
                assert_eq!(command_line.as_deref(), Some("echo hello world"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn markers_without_the_expected_token_are_ignored() {
        let mut terminal = state(40, 10);
        terminal.set_shell_token("secret".into());

        assert!(terminal.process_bytes(b"\x1b]133;A\x07").is_empty());
        assert!(
            terminal
                .process_bytes(b"\x1b]133;A;KATHAMO=other\x07")
                .is_empty()
        );

        let events = terminal.process_bytes(b"\x1b]133;A;KATHAMO=secret\x07");
        assert_eq!(kinds(&events), vec!["A"]);
    }

    #[test]
    fn markers_from_full_screen_apps_are_ignored() {
        let mut terminal = state(40, 10);
        terminal.process_bytes(b"\x1b[?1049h");
        let events = terminal.process_bytes(b"\x1b]133;A\x07\x1b]133;C\x07");
        assert!(
            events.is_empty(),
            "alt-screen markers must not create blocks"
        );

        terminal.process_bytes(b"\x1b[?1049l");
        let events = terminal.process_bytes(b"\x1b]133;A\x07");
        assert_eq!(kinds(&events), vec!["A"]);
    }

    #[test]
    fn alt_screen_outside_a_command_does_not_taint_the_next_block() {
        let mut terminal = state(40, 10);
        terminal.process_bytes(b"\x1b[?1049h\x1b[?1049l");
        terminal.process_bytes(b"\x1b]133;C\x07hello\r\n");
        let events = terminal.process_bytes(b"\x1b]133;D;0\x07");
        match &events[0] {
            ShellEvent::CommandFinished { full_screen, .. } => assert!(!*full_screen),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn render_output_lines_preserves_text_and_styles() {
        let palette = ColorPalette::default();
        let snapshot =
            render_output_lines(b"\x1b[31mred\x1b[0m plain\r\nsecond\r\n", 20, 4, &palette);
        assert_eq!(snapshot.lines.len(), 2);
        assert_eq!(snapshot.lines[0].runs[0].text, "red");
        assert!(snapshot.lines[0].runs[0].fg != snapshot.lines[0].runs[0].bg.unwrap_or_default());
        assert_eq!(snapshot.plain_text(), "red plain\nsecond");
    }

    #[test]
    fn render_output_lines_joins_hard_wraps() {
        let palette = ColorPalette::default();
        let snapshot = render_output_lines(b"abcdefghij\r\n", 5, 4, &palette);
        assert_eq!(snapshot.lines.len(), 1);
        assert_eq!(snapshot.plain_text(), "abcdefghij");
    }

    #[test]
    fn render_output_lines_handles_carriage_returns() {
        let palette = ColorPalette::default();
        // `\r` returns to column zero; later text overwrites earlier text in
        // place, exactly like a progress bar.
        let snapshot = render_output_lines(b"progress 10%\rprogress 50%\r\n", 30, 4, &palette);
        assert_eq!(snapshot.plain_text(), "progress 50%");
    }
}
