//! Editor component for multi-line text editing.
//!
//! Provides a text editor with cursor positioning, selection, and keyboard input.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::component::{Component, Focusable};
use super::keybindings::Keybindings;
use crate::ansi::CURSOR_MARKER;
use crate::kill_ring::{KillRing, PushOptions};
use crate::theme::Color;
use crate::undo_stack::UndoStack;

/// Editor style configuration.
#[derive(Debug, Clone)]
pub struct EditorStyle {
    pub prompt: String,
    pub placeholder: String,
}

impl Default for EditorStyle {
    fn default() -> Self {
        Self {
            prompt: "> ".to_string(),
            placeholder: String::new(),
        }
    }
}

/// Editor options.
#[derive(Debug, Clone, Default)]
pub struct EditorOptions {
    /// Padding on left and right.
    pub padding_x: usize,
    /// Maximum visible lines for autocomplete.
    pub autocomplete_max_visible: usize,
    /// Placeholder text when empty.
    pub placeholder: Option<String>,
    /// Initial text content.
    pub initial_text: Option<String>,
}

/// Editor state.
#[derive(Debug)]
struct EditorState {
    /// Lines of text.
    lines: Vec<String>,
    /// Cursor row (line index).
    cursor_row: usize,
    /// Cursor column (character index within line).
    cursor_col: usize,
    /// Selection anchor (row, col) if any.
    selection_anchor: Option<(usize, usize)>,
    /// Scroll offset for multi-line.
    #[allow(dead_code)]
    scroll_offset: usize,
    /// Whether the editor is focused.
    focused: bool,
    /// History of submitted lines.
    history: Vec<String>,
    /// History index for navigation.
    history_index: usize,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            selection_anchor: None,
            scroll_offset: 0,
            focused: false,
            history: Vec::new(),
            history_index: 0,
        }
    }
}

/// Owned paint inputs only: submitted history must never be copied for a frame.
#[derive(Debug, PartialEq, Eq)]
struct EditorRenderSnapshot {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    selection_anchor: Option<(usize, usize)>,
    focused: bool,
}

impl From<&EditorState> for EditorRenderSnapshot {
    fn from(state: &EditorState) -> Self {
        Self {
            lines: state.lines.clone(),
            cursor_row: state.cursor_row,
            cursor_col: state.cursor_col,
            selection_anchor: state.selection_anchor,
            focused: state.focused,
        }
    }
}

/// Greatest char-boundary byte index in `s` that is `<= idx` (clamped to
/// `s.len()`). The editor tracks `cursor_col` as a **byte** index and keeps it
/// on a char boundary; this helper restores that invariant when a caller
/// supplies a column that may be mid-character (e.g. clamping after moving
/// across lines of differing widths) and when converting external char counts.
///
/// Why byte indices: every `String` mutation the editor performs
/// (`insert`, `remove`, `truncate`, `drain`) and the `len()` comparisons used
/// for clamping are byte-indexed in `std`. Treating `cursor_col` as a byte
/// offset (advanced by `ch.len_utf8()`, never `+1`) keeps all of those sound
/// for multibyte input. Movement ops use this helper to step by *character*
/// while storing bytes.
fn snap_boundary(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    // A valid char boundary (including the END of the string) is kept as-is —
    // the old `take_while(b <= idx)` returned the START of the char AT `idx`
    // for boundary positions, so a caret placed exactly after the last char
    // (e.g. autocomplete accept: `set_cursor(0, len)`) snapped one char EARLY
    // and the terminal showed "/mode|l" with the final char dangling past the
    // caret. Mid-character positions snap DOWN to the char start instead.
    if s.is_char_boundary(idx) {
        return idx;
    }
    s.char_indices()
        .take_while(|(b, _)| *b < idx)
        .last()
        .map(|(b, _)| b)
        .unwrap_or(0)
}

/// A folded store of a large paste, keyed by the id rendered in its marker.
/// The document (`EditorState::lines`) only ever carries the short
/// `[paste #id …]` marker; the real text lives here until the draft is
/// submitted, at which point [`Editor::get_expanded_text`] splices it back in.
#[derive(Debug, Clone)]
struct PasteEntry {
    // Undo/redo snapshots share unchanged bodies instead of copying potentially
    // large pastes for every edit. Incremental pastes replace the Arc so earlier
    // snapshots remain immutable.
    content: Arc<str>,
}

/// An exclusive continuation token for one incrementally delivered paste.
///
/// The caller must establish that subsequent chunks belong to the same paste.
/// Any intervening edit, key event, or explicit cursor placement invalidates
/// the token; it is deliberately neither cloneable nor part of undo history.
#[derive(Debug)]
pub struct PasteSession {
    owner: Arc<AtomicU64>,
    revision: u64,
    start: (usize, usize),
    end: (usize, usize),
    display: String,
    raw: String,
    paste_id: Option<u32>,
    undo_started: bool,
}

/// Tab stops the editor expands pasted tabs to, in visible columns. 8 is
/// the POSIX/terminal default and matches how the source the user copied
/// from renders, so the paste lines up with what they saw.
pub(crate) const PASTE_TAB_WIDTH: usize = 8;

/// Expand tabs to spaces that advance to the next tab stop, matching how a
/// terminal renders them. `col` is the visible column count already consumed
/// on the current line; wide chars count 2 columns (not chars), and a wide
/// char straddling a stop jumps past it without partial padding.
///
/// Width is measured on each complete tab-free span with
/// `UnicodeWidthStr::width`, not by summing grapheme (or scalar) widths.
/// `UnicodeWidthStr` is not additive across cluster boundaries: with the
/// locked unicode-width 0.1.14, `❤️` (VS16 presentation sequence) is 2
/// columns as a whole but 1 summed scalar-wise, `👩‍💻` (ZWJ sequence) is 2 as a
/// whole but 4 summed scalar-wise, and the Arabic Lam-Alef ligature `لا` is 1
/// column as a whole but 2 summed grapheme-wise. Measuring anything smaller
/// than the span therefore shifted the tab stop — and the text sent to the
/// model, since the expanded paste is what gets sent.
pub(crate) fn expand_tabs(text: &str, tab_width: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut col = 0usize;
    // Width is a property of the whole run of text between tabs and newlines,
    // so buffer it and measure once instead of per grapheme.
    let mut span = String::new();
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        match grapheme {
            "\n" => {
                out.push_str(&span);
                span.clear();
                out.push('\n');
                col = 0;
            }
            "\t" => {
                col += UnicodeWidthStr::width(span.as_str());
                out.push_str(&span);
                span.clear();
                let next = (col / tab_width + 1) * tab_width;
                out.extend(std::iter::repeat(' ').take(next - col));
                col = next;
            }
            _ => span.push_str(grapheme),
        }
    }
    out.push_str(&span);
    out
}

/// Apply exactly the same normalization to ordinary and incremental pastes.
/// Incremental callers normalize the accumulated raw text, so even CRLF and
/// CSI-u sequences split across chunks are handled as a single payload.
fn normalize_paste(text: &str) -> String {
    // Tabs expand to the next tab stop per visible column rather than a fixed
    // width, so the paste renders the way it did where it was copied from.
    let decoded = decode_csi_u_ctrl(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let decoded = expand_tabs(&decoded, PASTE_TAB_WIDTH);
    decoded
        .chars()
        .filter(|c| *c == '\n' || (*c as u32) >= 32)
        .collect()
}

/// Replace only the specified byte-boundary span, retaining both surrounding
/// fragments and all other lines. Returns the end of the replacement.
fn replace_paste_range(
    state: &mut EditorState,
    start: (usize, usize),
    end: (usize, usize),
    text: &str,
) -> (usize, usize) {
    let (sr, sc) = start;
    let (er, ec) = end;
    let prefix = state.lines[sr][..sc].to_string();
    let suffix = state.lines[er][ec..].to_string();
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let row = sr + lines.len() - 1;
    let col = lines.last().unwrap().len() + if row == sr { sc } else { 0 };
    lines[0].insert_str(0, &prefix);
    lines.last_mut().unwrap().push_str(&suffix);
    state.lines.splice(sr..=er, lines);
    state.cursor_row = row;
    state.cursor_col = col;
    state.selection_anchor = None;
    (row, col)
}

/// Read just the owned span, checking bounds and UTF-8 boundaries first.
fn paste_range_text(
    state: &EditorState,
    start: (usize, usize),
    end: (usize, usize),
) -> Option<String> {
    let (sr, sc) = start;
    let (er, ec) = end;
    if start > end {
        return None;
    }
    if sr == er {
        return Some(state.lines.get(sr)?.get(sc..ec)?.to_string());
    }
    let mut text = state.lines.get(sr)?.get(sc..)?.to_string();
    for row in sr + 1..er {
        text.push('\n');
        text.push_str(state.lines.get(row)?);
    }
    text.push('\n');
    text.push_str(state.lines.get(er)?.get(..ec)?);
    Some(text)
}

/// Matches a whole `[paste #id …]` marker (group 1 = id).
fn paste_marker_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\[paste #(\d+)( (\+\d+ lines|\d+ chars))?\]").unwrap())
}

/// The byte length of the `[paste #id …]` marker ending exactly at `end`
/// (`end` is a byte offset in `line`), or `0` when no marker ends there.
fn paste_marker_len_ending_at(line: &str, end: usize) -> usize {
    for m in paste_marker_regex().find_iter(line) {
        if m.end() == end {
            return m.as_str().len();
        }
    }
    0
}

/// A paste folds when it exceeds either threshold (upstream's `handlePaste`).
const PASTE_FOLD_LINE_THRESHOLD: usize = 10;
const PASTE_FOLD_CHAR_THRESHOLD: usize = 1000;

/// Replace every `[paste #id …]` marker in `document` with its stashed body
/// (upstream's `expandPasteMarkers`). Markers whose id is absent from
/// `pastes` are left as-is so a stray marker never silently vanishes.
fn expand_paste_markers(document: &str, pastes: &HashMap<u32, PasteEntry>) -> String {
    if pastes.is_empty() || !document.contains("[paste #") {
        return document.to_string();
    }
    paste_marker_regex()
        .replace_all(document, |caps: &regex::Captures| {
            let id: u32 = caps[1].parse().unwrap_or(0);
            match pastes.get(&id) {
                Some(entry) => entry.content.to_string(),
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

/// Decode the kitty CSI-u form a client may emit for `Ctrl+<letter>`
/// (`ESC[<code>;5u`), which upstream decodes before folding
/// (`handlePaste`). Scalars that are not a `Ctrl+letter` chord are left as-is.
fn decode_csi_u_ctrl(text: &str) -> String {
    if !text.contains("\x1b[") {
        return text.to_string();
    }
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\x1b\[(\d+);5u").unwrap());
    re.replace_all(text, |caps: &regex::Captures| {
        let code: u32 = caps[1].parse().unwrap_or(0);
        let lower = 0x61..=0x7a;
        let upper = 0x41..=0x5a;
        if lower.contains(&code) {
            char::from_u32(code - 96)
                .map(String::from)
                .unwrap_or_default()
        } else if upper.contains(&code) {
            char::from_u32(code - 64)
                .map(String::from)
                .unwrap_or_default()
        } else {
            caps[0].to_string()
        }
    })
    .into_owned()
}

/// Split a logical editor line into terminal-width chunks. The editor keeps
/// logical lines for editing and cursor movement; wrapping is strictly a
/// rendering concern so a long draft cannot escape the bordered input box.
fn wrap_line_ranges(line: &str, max_width: usize) -> Vec<(usize, usize)> {
    if line.is_empty() {
        return vec![(0, 0)];
    }
    let max_width = max_width.max(1);
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut width = 0;
    for (idx, ch) in line.char_indices() {
        let char_width = UnicodeWidthStr::width(ch.to_string().as_str());
        if idx > start && width + char_width > max_width {
            ranges.push((start, idx));
            start = idx;
            width = 0;
        }
        width += char_width;
    }
    ranges.push((start, line.len()));
    ranges
}

/// Insert the zero-width cursor marker at a visible column without slicing
/// through ANSI escape sequences added for selection/placeholder styling.
fn insert_cursor_marker(text: &str, column: usize) -> String {
    let mut out = String::with_capacity(text.len() + CURSOR_MARKER.len());
    let mut visible = 0;
    let mut chars = text.char_indices().peekable();
    let mut inserted = false;
    while let Some((idx, ch)) = chars.next() {
        if ch == '\x1b' {
            out.push(ch);
            for (_, esc) in chars.by_ref() {
                out.push(esc);
                if esc == '\x07' || ('@'..='~').contains(&esc) {
                    break;
                }
            }
            continue;
        }
        if !inserted && visible >= column {
            out.push_str(CURSOR_MARKER);
            inserted = true;
        }
        out.push(ch);
        visible += UnicodeWidthStr::width(ch.to_string().as_str());
        let _ = idx;
    }
    if !inserted {
        out.push_str(CURSOR_MARKER);
    }
    out
}

/// Editor - A multi-line text editor component.
pub struct Editor {
    state: Mutex<EditorState>,
    options: EditorOptions,
    #[allow(dead_code)]
    style: EditorStyle,
    #[allow(dead_code)]
    keybindings: Arc<Keybindings>,
    on_submit: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    on_change: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    /// Undo history (text + caret snapshots). Pushed before each mutation;
    /// consecutive character insertions coalesce into one entry.
    undo_stack: Mutex<UndoStack<EditorSnapshot>>,
    /// Popped undo entries, for Ctrl+R redo.
    redo_stack: Mutex<Vec<EditorSnapshot>>,
    /// Emacs-style kill ring for Ctrl+K/U/W kills; Ctrl+Y yanks.
    kill_ring: Mutex<KillRing>,
    /// The last mutation kind, for undo coalescing + kill accumulation.
    last_action: Mutex<Option<&'static str>>,
    /// Character-jump mode (pi `jumpForward`/`jumpBackward`): `Some(1)` waits
    /// for the next printable char to jump forward to, `Some(-1)` backward.
    /// The next character input consumes it instead of inserting.
    jump_mode: Mutex<Option<i32>>,
    /// Explicit top/bottom border color. `None` ⇒ the theme's default border
    /// color. Upstream's `editor.borderColor` (used for bash mode and thinking
    /// levels via `updateEditorBorderColor`).
    border_color: Mutex<Option<Color>>,
    /// Working indicator text shown inside the top border. `None` means idle.
    /// When `Some`, the top border line renders the spinner + elapsed + message
    /// instead of plain `─` characters (pi's working-inside-input-border style).
    working: Mutex<Option<WorkingState>>,
    /// Folded pastes: `id -> content`, referenced by a `[paste #id …]` marker
    /// in `state.lines`. Mirrors upstream's `pastes` map.
    pastes: Mutex<HashMap<u32, PasteEntry>>,
    /// Monotonic id source for [`Self::pastes`] (upstream's `pasteCounter`).
    paste_counter: Mutex<u32>,
    /// Unique owner identity and monotonic invalidation revision. Never restored
    /// from an undo snapshot, so undo/redo cannot revive an old paste token.
    paste_revision: Arc<AtomicU64>,
}

/// State for the working indicator embedded in the editor's top border.
#[derive(Debug, Clone)]
pub struct WorkingState {
    /// When the working state started. Also seeds the spinner: the frame is
    /// derived from elapsed time (one step per `SPINNER_FRAME_MS`), matching
    /// upstream's fixed-cadence `setInterval` animation regardless of how
    /// often repaints land.
    pub started_at: std::time::Instant,
    /// The message to display (e.g. "Working…").
    pub message: String,
}

/// A restorable editor state snapshot (text + caret, plus the folded paste
/// store so undo can resurrect a marker's content). Selection/scroll/focus
/// are not part of undo (mirrors the TS `EditorSnapshot`).
#[derive(Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    pastes: HashMap<u32, PasteEntry>,
    paste_counter: u32,
}

impl Editor {
    /// Create a new editor with options.
    pub fn new(options: EditorOptions, style: EditorStyle, keybindings: Arc<Keybindings>) -> Self {
        let mut state = EditorState::default();
        if let Some(text) = &options.initial_text {
            state.lines = text.split('\n').map(|s| s.to_string()).collect();
            if state.lines.is_empty() {
                state.lines.push(String::new());
            }
        }

        Self {
            state: Mutex::new(state),
            options,
            style,
            keybindings,
            on_submit: Mutex::new(None),
            on_change: Mutex::new(None),
            undo_stack: Mutex::new(UndoStack::with_max_size(200)),
            redo_stack: Mutex::new(Vec::new()),
            kill_ring: Mutex::new(KillRing::new()),
            last_action: Mutex::new(None),
            jump_mode: Mutex::new(None),
            border_color: Mutex::new(None),
            working: Mutex::new(None),
            pastes: Mutex::new(HashMap::new()),
            paste_counter: Mutex::new(0),
            paste_revision: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Create a new editor with default options.
    pub fn simple() -> Self {
        Self::new(
            EditorOptions::default(),
            EditorStyle::default(),
            Arc::new(Keybindings::new()),
        )
    }

    /// Set the text content.
    pub fn set_text(&self, text: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.lines = if text.is_empty() {
                vec![String::new()]
            } else {
                text.split('\n').map(|s| s.to_string()).collect()
            };
            if state.lines.is_empty() {
                state.lines.push(String::new());
            }
            state.cursor_row = 0;
            state.cursor_col = 0;
        }
        // A programmatic replacement discards any folded pastes: the new text
        // is the whole truth (mirrors upstream's `setText` clearing `pastes`).
        self.reset_pastes();
        // Mirrors native `setTextInternal`: programmatic replacement is a change.
        self.notify_change();
    }

    /// Get the text content **as shown**: folded large pastes appear as their
    /// `[paste #id …]` marker, not their full body. This is the right source
    /// for UI state that reasons about what is on screen (autocomplete,
    /// empty-checks). Use [`Self::get_expanded_text`] for anything that must
    /// see the user's real content.
    pub fn get_text(&self) -> String {
        self.state
            .lock()
            .map(|s| s.lines.join("\n"))
            .unwrap_or_default()
    }

    /// Get the text content **with folded pastes expanded** (upstream's
    /// `getExpandedText`): every `[paste #id …]` marker is replaced by the full
    /// pasted body. Submission, the external editor and extension reads all go
    /// through here so a folded paste is never sent truncated.
    pub fn get_expanded_text(&self) -> String {
        let document = self.get_text();
        let pastes = self.pastes.lock().map(|p| p.clone()).unwrap_or_default();
        expand_paste_markers(&document, &pastes)
    }

    /// Clear the folded-paste store and reset the id counter.
    fn reset_pastes(&self) {
        if let Ok(mut pastes) = self.pastes.lock() {
            pastes.clear();
        }
        if let Ok(mut counter) = self.paste_counter.lock() {
            *counter = 0;
        }
    }

    /// The folded-paste id whose marker starts exactly at byte offset
    /// `start` on line `row`, if any.
    fn paste_id_at_marker_start(&self, row: usize, start: usize) -> Option<u32> {
        let state = self.state.lock().ok()?;
        let line = state.lines.get(row)?;
        paste_marker_regex()
            .captures_iter(line)
            .find(|caps| caps.get(0).unwrap().start() == start)
            .and_then(|caps| caps[1].parse::<u32>().ok())
    }

    /// Drop a folded paste and renumber the higher ids down by one so the
    /// markers left in the document stay contiguous (upstream renumbers in
    /// `handleBackspace`). Also rewrites the surviving markers in place.
    fn remove_paste(&self, id: u32) {
        {
            let mut pastes = self.pastes.lock().unwrap();
            if pastes.remove(&id).is_none() {
                return;
            }
            let higher: Vec<u32> = pastes.keys().copied().filter(|k| *k > id).collect();
            for old in higher {
                if let Some(entry) = pastes.remove(&old) {
                    pastes.insert(old - 1, entry);
                }
            }
        }
        {
            let mut counter = self.paste_counter.lock().unwrap();
            *counter = counter.saturating_sub(1);
        }
        // Rewrite the markers still in the document: ids above the removed one
        // shift down by one, matching the renumbered store.
        if let Ok(mut state) = self.state.lock() {
            for line in state.lines.iter_mut() {
                *line = paste_marker_regex()
                    .replace_all(line, |caps: &regex::Captures| {
                        let n: u32 = caps[1].parse().unwrap_or(0);
                        if n <= id {
                            caps[0].to_string()
                        } else {
                            // Group 2 already carries the leading space + suffix.
                            let suffix = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                            format!("[paste #{}{suffix}]", n - 1)
                        }
                    })
                    .into_owned();
            }
        }
    }

    /// Fold a pasted block: short pastes are inserted verbatim; a block over
    /// the size threshold is stashed and replaced by a `[paste #id …]` marker
    /// so the input box stays readable. Mirrors upstream's `handlePaste`
    /// (fold when `>10` lines or `>1000` chars).
    ///
    /// A single-line pasted file path is special-cased by the caller (image
    /// attachment), so this only handles text.
    pub fn handle_paste(&self, text: &str) {
        let filtered = normalize_paste(text);
        if filtered.is_empty() {
            return;
        }

        let line_count = filtered.split('\n').count();
        let char_count = filtered.chars().count();
        if line_count > PASTE_FOLD_LINE_THRESHOLD || char_count > PASTE_FOLD_CHAR_THRESHOLD {
            self.push_undo("insert");
            if self.has_selection() {
                self.delete_selection_no_undo();
            }
            let (id, by_lines, count) = {
                let mut counter = self.paste_counter.lock().unwrap();
                *counter += 1;
                let id = *counter;
                if line_count > PASTE_FOLD_LINE_THRESHOLD {
                    (id, true, line_count)
                } else {
                    (id, false, char_count)
                }
            };
            if let Ok(mut pastes) = self.pastes.lock() {
                pastes.insert(
                    id,
                    PasteEntry {
                        content: filtered.into(),
                    },
                );
            }
            let marker = if by_lines {
                format!("[paste #{id} +{count} lines]")
            } else {
                format!("[paste #{id} {count} chars]")
            };
            self.insert_no_undo(&marker);
            *self.last_action.lock().unwrap() = Some("insert");
        } else {
            self.insert(&filtered);
        }
    }

    /// Insert the first chunk of a controlled paste immediately.
    ///
    /// This paste has its own undo boundary, separate from preceding/following
    /// typing. Keep the returned token only while the input source can prove
    /// further chunks belong to this same paste.
    pub fn start_paste_session(&self, text: &str) -> PasteSession {
        self.paste_revision.fetch_add(1, Ordering::SeqCst);
        let normalized = normalize_paste(text);
        let mut session = PasteSession {
            owner: Arc::clone(&self.paste_revision),
            revision: self.paste_revision.load(Ordering::SeqCst),
            start: self.cursor_position(),
            end: self.cursor_position(),
            display: String::new(),
            raw: text.to_string(),
            paste_id: None,
            undo_started: !normalized.is_empty(),
        };
        if session.undo_started {
            self.push_undo("paste");
            let mut state = self.state.lock().unwrap();
            let cursor = (state.cursor_row, state.cursor_col);
            let (start, end) = match state.selection_anchor {
                Some(anchor) => (anchor.min(cursor), anchor.max(cursor)),
                None => (cursor, cursor),
            };
            session.start = start;
            let display = self.paste_session_display(&mut session, &normalized);
            session.end = replace_paste_range(&mut state, start, end, &display);
            session.display = display;
            drop(state);
        }
        *self.last_action.lock().unwrap() = Some("paste");
        if session.undo_started {
            // Capture the revision BEFORE calling observers. An observer that
            // edits/moves the editor must invalidate this token too.
            session.revision = self.paste_revision.load(Ordering::SeqCst).wrapping_add(1);
            self.notify_change();
        }
        session
    }

    /// Append a chunk known by the caller to belong to `session`'s paste.
    ///
    /// Returns `false` without changing the document, paste store, undo history,
    /// or token if any intervening action invalidated it. In that case the
    /// caller should insert the chunk as an independent paste instead.
    pub fn append_paste_session(&self, session: &mut PasteSession, text: &str) -> bool {
        self.append_paste_session_inner(session, text, true)
    }

    /// Display a not-yet-corroborated continuation without folding it.
    ///
    /// Retains ownership for later confirmation, but never hides these bytes
    /// inside an existing folded paste or updates its stored content. Dropping
    /// the token leaves all received text visible and intact.
    pub fn append_paste_session_unfolded(&self, session: &mut PasteSession, text: &str) -> bool {
        self.append_paste_session_inner(session, text, false)
    }

    fn append_paste_session_inner(
        &self,
        session: &mut PasteSession,
        text: &str,
        fold: bool,
    ) -> bool {
        if !Arc::ptr_eq(&session.owner, &self.paste_revision)
            || session.revision != self.paste_revision.load(Ordering::SeqCst)
        {
            return false;
        }
        let mut raw = session.raw.clone();
        raw.push_str(text);
        let normalized = normalize_paste(&raw);
        // Validate immediately before writing, and hold the state lock through
        // the bounded replacement rather than releasing it after validation.
        let mut state = self.state.lock().unwrap();
        if session.revision != self.paste_revision.load(Ordering::SeqCst)
            || state.selection_anchor.is_some()
            || (state.cursor_row, state.cursor_col) != session.end
            || paste_range_text(&state, session.start, session.end).as_deref()
                != Some(session.display.as_str())
        {
            return false;
        }
        // A provisional suffix stays outside the stored marker until the
        // caller confirms it. Never rewrite an already-folded prefix merely
        // because normalization of an unverified suffix would change it.
        let provisional = if fold {
            None
        } else if let Some(id) = session.paste_id {
            let pastes = self.pastes.lock().unwrap();
            let Some(entry) = pastes.get(&id) else {
                return false;
            };
            let Some(suffix) = normalized.strip_prefix(entry.content.as_ref()) else {
                return false;
            };
            let Some(marker) = paste_marker_regex()
                .find(&session.display)
                .filter(|m| m.start() == 0)
            else {
                return false;
            };
            Some(format!("{}{suffix}", marker.as_str()))
        } else {
            Some(normalized.clone())
        };
        // Empty/control-only first chunks must not consume a selection or
        // introduce an undo entry. Defer that entry until something is inserted.
        if !session.undo_started && normalized.is_empty() {
            session.raw = raw;
            return true;
        }
        if !session.undo_started {
            let snapshot = self.snapshot_locked_state(&state);
            self.undo_stack.lock().unwrap().push(snapshot);
            self.redo_stack.lock().unwrap().clear();
            session.undo_started = true;
        }
        let display =
            provisional.unwrap_or_else(|| self.paste_session_display(session, &normalized));
        session.end = replace_paste_range(&mut state, session.start, session.end, &display);
        session.display = display;
        session.raw = raw;
        drop(state);
        *self.last_action.lock().unwrap() = Some("paste");
        session.revision = self.paste_revision.load(Ordering::SeqCst).wrapping_add(1);
        self.notify_change();
        true
    }

    /// Produce the owned display and update only this session's store entry.
    /// Once folded, keep the same id even if normalization of a split escape
    /// sequence changes the size of the accumulated payload.
    fn paste_session_display(&self, session: &mut PasteSession, normalized: &str) -> String {
        let lines = normalized.split('\n').count();
        let chars = normalized.chars().count();
        if session.paste_id.is_none()
            && lines <= PASTE_FOLD_LINE_THRESHOLD
            && chars <= PASTE_FOLD_CHAR_THRESHOLD
        {
            return normalized.to_string();
        }
        let id = *session.paste_id.get_or_insert_with(|| {
            let mut counter = self.paste_counter.lock().unwrap();
            *counter += 1;
            *counter
        });
        self.pastes.lock().unwrap().insert(
            id,
            PasteEntry {
                content: normalized.into(),
            },
        );
        if lines > PASTE_FOLD_LINE_THRESHOLD {
            format!("[paste #{id} +{lines} lines]")
        } else {
            format!("[paste #{id} {chars} chars]")
        }
    }

    /// Clear the editor.
    pub fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.lines = vec![String::new()];
            state.cursor_row = 0;
            state.cursor_col = 0;
            state.selection_anchor = None;
        }
        self.reset_pastes();
        // Upstream's editor notifies change observers for programmatic clears
        // (`setTextInternal`), so derived UI state (bash-mode border color,
        // autocomplete) drops the stale draft. `submit()` relies on this to
        // reset the border after routing a `!command`.
        self.notify_change();
    }

    /// Set callback for when text is submitted (Enter pressed).
    pub fn on_submit(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_submit.lock() {
            *cb = Some(callback);
        }
    }

    /// Set callback for when text changes.
    pub fn on_change(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_change.lock() {
            *cb = Some(callback);
        }
    }

    /// Get cursor position.
    pub fn cursor_position(&self) -> (usize, usize) {
        self.state
            .lock()
            .map(|s| (s.cursor_row, s.cursor_col))
            .unwrap_or((0, 0))
    }

    /// Number of logical lines in the draft (always at least 1).
    pub fn line_count(&self) -> usize {
        self.state.lock().map(|s| s.lines.len()).unwrap_or(1).max(1)
    }

    /// Set the cursor position (clamped to text bounds), clearing any
    /// in-progress selection. Used by autocomplete to place the caret after
    /// accepting a suggestion (`set_text` resets the caret to the start).
    ///
    /// `col` is interpreted as a **byte** offset (callers in the autocomplete
    /// path pass byte indices from `String::len`/`char_indices`) and snapped to
    /// the nearest preceding char boundary so multibyte input never lands
    /// mid-character.
    pub fn set_cursor(&self, row: usize, col: usize) {
        self.paste_revision.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut state) = self.state.lock() {
            let row = row.min(state.lines.len().saturating_sub(1));
            let col = snap_boundary(&state.lines[row], col);
            state.cursor_row = row;
            state.cursor_col = col;
            state.selection_anchor = None;
        }
    }

    /// Copy only paint inputs and release the state mutex before rendering.
    fn render_snapshot(&self) -> EditorRenderSnapshot {
        self.state
            .lock()
            .map(|state| EditorRenderSnapshot::from(&*state))
            .unwrap_or_else(|_| EditorRenderSnapshot::from(&EditorState::default()))
    }

    /// Snapshot the current text + caret state (for undo/redo), including the
    /// folded-paste store so undo can bring a dropped paste back.
    fn snapshot_state(&self) -> EditorSnapshot {
        let state = self.state.lock().unwrap();
        self.snapshot_locked_state(&state)
    }

    /// Snapshot while the caller already holds the document lock.
    fn snapshot_locked_state(&self, state: &EditorState) -> EditorSnapshot {
        EditorSnapshot {
            lines: state.lines.clone(),
            cursor_row: state.cursor_row,
            cursor_col: state.cursor_col,
            pastes: self.pastes.lock().map(|p| p.clone()).unwrap_or_default(),
            paste_counter: self.paste_counter.lock().map(|c| *c).unwrap_or(0),
        }
    }

    /// Restore a snapshot, notifying change observers.
    fn restore_state(&self, snap: &EditorSnapshot) {
        if let Ok(mut state) = self.state.lock() {
            state.lines = snap.lines.clone();
            state.cursor_row = snap.cursor_row;
            state.cursor_col = snap.cursor_col;
            // Snapshots don't carry the selection; undo/redo ends it.
            state.selection_anchor = None;
        }
        if let Ok(mut pastes) = self.pastes.lock() {
            *pastes = snap.pastes.clone();
        }
        if let Ok(mut counter) = self.paste_counter.lock() {
            *counter = snap.paste_counter;
        }
        self.notify_change();
    }

    /// Record an undo snapshot before a mutation. `action` names the mutation
    /// kind: consecutive `"insert"` actions coalesce into a single undo entry
    /// (typing a word undoes as one step, mirroring the TS coalescing); any
    /// other mutation starts a fresh entry. A new mutation clears the redo
    /// stack (the classic undo/redo semantics).
    fn push_undo(&self, action: &'static str) {
        let coalesce = action == "insert" && *self.last_action.lock().unwrap() == Some("insert");
        if !coalesce {
            let snap = self.snapshot_state();
            if let Ok(mut stack) = self.undo_stack.lock() {
                stack.push(snap);
            }
            if let Ok(mut redo) = self.redo_stack.lock() {
                redo.clear();
            }
        }
        *self.last_action.lock().unwrap() = Some(action);
    }

    /// Undo the last mutation (Ctrl+-). Restores the prior snapshot; the
    /// undone state moves to the redo stack for Ctrl+R.
    pub fn undo(&self) {
        self.paste_revision.fetch_add(1, Ordering::SeqCst);
        let popped = self.undo_stack.lock().unwrap().pop();
        let Some(snap) = popped else { return };
        // The current state is the redo target.
        self.redo_stack.lock().unwrap().push(self.snapshot_state());
        self.restore_state(&snap);
        *self.last_action.lock().unwrap() = None;
    }

    /// Redo the last undone mutation (Ctrl+R).
    pub fn redo(&self) {
        self.paste_revision.fetch_add(1, Ordering::SeqCst);
        let popped = self.redo_stack.lock().unwrap().pop();
        let Some(snap) = popped else { return };
        self.undo_stack.lock().unwrap().push(self.snapshot_state());
        self.restore_state(&snap);
        *self.last_action.lock().unwrap() = None;
    }

    /// Push killed (deleted) text onto the kill ring, accumulating consecutive
    /// kills into one entry (mirrors the TS kill-ring semantics).
    fn kill(&self, text: String, prepend: bool) {
        if text.is_empty() {
            return;
        }
        let accumulate = *self.last_action.lock().unwrap() == Some("kill");
        if let Ok(mut ring) = self.kill_ring.lock() {
            ring.push(
                &text,
                PushOptions {
                    prepend,
                    accumulate,
                },
            );
        }
        *self.last_action.lock().unwrap() = Some("kill");
    }

    /// Yank (paste) the most recent kill at the cursor (Ctrl+Y). Replaces an
    /// active selection.
    pub fn yank(&self) {
        let text = self.kill_ring.lock().unwrap().peek().map(str::to_string);
        let Some(text) = text else { return };
        self.push_undo("yank");
        if self.has_selection() {
            self.delete_selection_no_undo();
        }
        self.insert_no_undo(&text);
        *self.last_action.lock().unwrap() = Some("yank");
    }

    /// Yank-pop: rotate the kill ring and yank the next entry (Alt+Y).
    pub fn yank_pop(&self) {
        if self.kill_ring.lock().unwrap().len() <= 1 {
            return;
        }
        self.kill_ring.lock().unwrap().rotate();
        self.yank();
    }

    // -------------------------------------------------------------------
    // Selection (Shift+arrows select, Ctrl+X/C/V cut/copy/paste).
    // -------------------------------------------------------------------

    /// Whether a selection is active.
    pub fn has_selection(&self) -> bool {
        self.state.lock().unwrap().selection_anchor.is_some()
    }

    /// The ordered selection span `(start_row, start_col, end_row, end_col)`
    /// with start <= end (byte offsets, both on char boundaries).
    fn selection_ordered(&self) -> Option<(usize, usize, usize, usize)> {
        let state = self.state.lock().unwrap();
        let (ar, ac) = state.selection_anchor?;
        let (br, bc) = (state.cursor_row, state.cursor_col);
        Some(if (ar, ac) <= (br, bc) {
            (ar, ac, br, bc)
        } else {
            (br, bc, ar, ac)
        })
    }

    /// The selected text ("\n"-joined across rows), if any.
    pub fn selected_text(&self) -> Option<String> {
        let (sr, sc, er, ec) = self.selection_ordered()?;
        let lines = self.state.lock().unwrap().lines.clone();
        if sr == er {
            Some(lines[sr][sc..ec].to_string())
        } else {
            let mut out = String::new();
            out.push_str(&lines[sr][sc..]);
            for r in sr + 1..er {
                out.push('\n');
                out.push_str(&lines[r]);
            }
            out.push('\n');
            out.push_str(&lines[er][..ec]);
            Some(out)
        }
    }

    /// The selection span on `row`, as `(start_col, end_col)` byte offsets,
    /// for render-time highlighting. `None` when the row is outside the span.
    fn selection_span_on_row(
        &self,
        state: &EditorRenderSnapshot,
        row: usize,
    ) -> Option<(usize, usize)> {
        let (ar, ac) = state.selection_anchor?;
        let (br, bc) = (state.cursor_row, state.cursor_col);
        let (sr, sc, er, ec) = if (ar, ac) <= (br, bc) {
            (ar, ac, br, bc)
        } else {
            (br, bc, ar, ac)
        };
        if row < sr || row > er {
            return None;
        }
        if sr == er {
            (sc != ec).then_some((sc, ec))
        } else if row == sr {
            (sc < state.lines[row].len()).then_some((sc, state.lines[row].len()))
        } else if row == er {
            (ec > 0).then_some((0, ec))
        } else {
            (!state.lines[row].is_empty()).then_some((0, state.lines[row].len()))
        }
    }

    /// Seed the selection anchor at the current caret (Shift+direction start).
    fn begin_selection(&self) {
        let mut state = self.state.lock().unwrap();
        if state.selection_anchor.is_none() {
            state.selection_anchor = Some((state.cursor_row, state.cursor_col));
        }
    }

    /// Clear the active selection (plain cursor movement / edits).
    fn clear_selection(&self) {
        self.state.lock().unwrap().selection_anchor = None;
    }

    /// Delete the selected span, leaving the caret at its start. Pushes an
    /// undo snapshot; no-op without a selection.
    fn delete_selection(&self) {
        if !self.has_selection() {
            return;
        }
        self.push_undo("edit");
        self.delete_selection_no_undo();
    }

    /// Delete the selected span WITHOUT an undo snapshot (caller pushed one,
    /// e.g. a replace-through-insert coalesced with the insertion).
    fn delete_selection_no_undo(&self) {
        let (sr, sc, er, ec) = match self.selection_ordered() {
            Some(s) => s,
            None => return,
        };
        let mut state = self.state.lock().unwrap();
        if sr == er {
            state.lines[sr].replace_range(sc..ec, "");
            state.cursor_row = sr;
            state.cursor_col = sc;
        } else {
            let mut joined = state.lines[sr][..sc].to_string();
            joined.push_str(&state.lines[er][ec..]);
            state.lines.drain(sr + 1..=er);
            state.lines[sr] = joined;
            state.cursor_row = sr;
            state.cursor_col = sc;
        }
        state.selection_anchor = None;
        drop(state);
        self.notify_change();
    }

    /// Copy the selection onto the kill ring (Ctrl+C). No-op without one.
    pub fn copy_selection(&self) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        if let Ok(mut ring) = self.kill_ring.lock() {
            ring.push(&text, PushOptions::default());
        }
        true
    }

    /// Cut the selection (copy + delete, Ctrl+X). Returns whether anything
    /// was cut.
    pub fn cut_selection(&self) -> bool {
        if !self.copy_selection() {
            return false;
        }
        self.delete_selection();
        true
    }

    /// Insert text at cursor position. Replaces an active selection (one undo
    /// step: the snapshot is taken before the selection is deleted).
    pub fn insert(&self, text: &str) {
        self.push_undo("insert");
        if self.has_selection() {
            self.delete_selection_no_undo();
        }
        self.insert_no_undo(text);
    }

    /// Insert without an undo snapshot (caller pushed one — yank/replace
    /// paths). Notifies change observers.
    ///
    /// Line endings are normalized: `\r\n` and a lone `\r` are treated as one
    /// newline. Pasted Windows text (and the `Event::Paste` payload on Unix,
    /// which preserves CRLF) would otherwise insert literal carriage returns
    /// into the line buffer — they render as garbage and break `cursor_col`
    /// arithmetic on the next edit.
    fn insert_no_undo(&self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Ok(mut state) = self.state.lock() {
            for ch in normalized.chars() {
                if ch == '\n' {
                    // Split line at cursor
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    let current_line = &mut state.lines[row];
                    let after = current_line.split_off(col);
                    state.lines.insert(row + 1, after);
                    state.cursor_row += 1;
                    state.cursor_col = 0;
                } else {
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    state.lines[row].insert(col, ch);
                    // Advance by the BYTE length of the inserted char so
                    // `cursor_col` stays a valid char boundary (and a sound
                    // byte index for the next `String` mutation). The old
                    // `+= 1` was a char count and panicked on multibyte input
                    // the moment a second keystroke landed mid-character.
                    state.cursor_col += ch.len_utf8();
                }
            }
        }
        self.notify_change();
    }

    /// Delete character before cursor (Backspace). With a selection, deletes
    /// the selection instead.
    fn backspace(&self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        // Backspacing over a `[paste #id …]` marker deletes the whole marker
        // (and drops / renumbers the folded pastes). The marker is one atomic
        // unit — the user cannot nibble it into an orphaned placeholder.
        let deleted_marker = {
            let state = self.state.lock().unwrap();
            let row = state.cursor_row;
            (state.cursor_col > 0)
                .then(|| paste_marker_len_ending_at(&state.lines[row], state.cursor_col))
                .filter(|len| *len > 0)
                .map(|len| (row, state.cursor_col - len, len))
        };
        if let Some((row, start, len)) = deleted_marker {
            // Snapshot FIRST: `remove_paste` mutates the paste store and
            // renumbers the surviving markers, so undo must capture the
            // pre-deletion state to be able to bring them all back.
            self.push_undo("edit");
            let id = self.paste_id_at_marker_start(row, start);
            // Delete the marker text BEFORE renumbering, so the renumber pass
            // (which can change a marker's byte length when an id crosses
            // `#10 -> #9`) never has to keep a stale `start..start+len` valid.
            if let Ok(mut state) = self.state.lock() {
                state.lines[row].replace_range(start..start + len, "");
                state.cursor_col = start;
            }
            if let Some(id) = id {
                self.remove_paste(id);
            }
            self.notify_change();
            return;
        }
        self.push_undo("edit");
        if let Ok(mut state) = self.state.lock() {
            if state.cursor_col > 0 {
                let row = state.cursor_row;
                // Step back by the byte length of the char immediately
                // before the cursor, then remove that char. (`remove` takes
                // a byte index and must land on a boundary.) Compute the
                // length through a shared borrow first so the mutable borrow
                // for `remove` is the only outstanding alias.
                let prev_len = state.lines[row][..state.cursor_col]
                    .chars()
                    .last()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                let col = state.cursor_col - prev_len;
                state.lines[row].remove(col);
                state.cursor_col = col;
            } else if state.cursor_row > 0 {
                // Join with previous line
                let row = state.cursor_row;
                let current = state.lines.remove(row);
                state.cursor_row -= 1;
                let prev_row = state.cursor_row;
                state.cursor_col = state.lines[prev_row].len();
                state.lines[prev_row].push_str(&current);
            }
        }
        self.notify_change();
    }

    /// Delete character at cursor (Delete). With a selection, deletes the
    /// selection instead.
    fn delete(&self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        self.push_undo("edit");
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            let line_len = state.lines[row].len();
            if state.cursor_col < line_len {
                let col = state.cursor_col;
                state.lines[row].remove(col);
            } else if row < state.lines.len() - 1 {
                // Join with next line
                let next = state.lines.remove(row + 1);
                state.lines[row].push_str(&next);
            }
        }
        self.notify_change();
    }

    /// Move cursor left.
    fn cursor_left(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor_col > 0 {
                let row = state.cursor_row;
                let line = &state.lines[row];
                // A folded paste marker is atomic: step the whole marker.
                let marker = paste_marker_len_ending_at(line, state.cursor_col);
                if marker > 0 {
                    state.cursor_col -= marker;
                    return;
                }
                // Retrear by the byte length of the preceding char so the
                // caret stays on a char boundary.
                let prev_len = line[..state.cursor_col]
                    .chars()
                    .last()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                state.cursor_col -= prev_len;
            } else if state.cursor_row > 0 {
                state.cursor_row -= 1;
                state.cursor_col = state.lines[state.cursor_row].len();
            }
        }
    }

    /// Move cursor right.
    fn cursor_right(&self) {
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            let line = &state.lines[row];
            if state.cursor_col < line.len() {
                // A folded paste marker is atomic: step the whole marker.
                for m in paste_marker_regex().find_iter(line) {
                    if m.start() == state.cursor_col {
                        state.cursor_col = m.end();
                        return;
                    }
                }
                // Advance by the byte length of the char at the cursor.
                let ch_len = line[state.cursor_col..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                state.cursor_col += ch_len;
            } else if row < state.lines.len() - 1 {
                state.cursor_row += 1;
                state.cursor_col = 0;
            }
        }
    }

    /// Move cursor up.
    fn cursor_up(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor_row > 0 {
                state.cursor_row -= 1;
                let new_row = state.cursor_row;
                let max = state.lines[new_row].len();
                // The byte column may fall mid-character in the new (shorter
                // or differently-encoded) line; snap to a boundary.
                state.cursor_col = snap_boundary(&state.lines[new_row], state.cursor_col.min(max));
            }
        }
    }

    /// Move cursor down.
    fn cursor_down(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor_row < state.lines.len() - 1 {
                state.cursor_row += 1;
                let new_row = state.cursor_row;
                let max = state.lines[new_row].len();
                state.cursor_col = snap_boundary(&state.lines[new_row], state.cursor_col.min(max));
            }
        }
    }

    /// Move cursor to start of line.
    fn cursor_home(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.cursor_col = 0;
        }
    }

    /// Move cursor to end of line.
    fn cursor_end(&self) {
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            state.cursor_col = state.lines[row].len();
        }
    }

    /// Kill the word before the cursor (Ctrl+W / Alt+Backspace): Emacs
    /// backward-kill-word — skip the current word, then the intervening
    /// delimiters, landing on the previous word's start.
    fn kill_word_backward(&self) {
        self.push_undo("kill");
        let killed = if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            let col = state.cursor_col;
            let start = crate::word_navigation::find_word_backward(&state.lines[row], col);
            let dead: String = state.lines[row].drain(start..col).collect();
            state.cursor_col = start;
            dead
        } else {
            String::new()
        };
        self.kill(killed, true);
        self.notify_change();
    }

    /// Kill the word after the cursor (Alt+D / Alt+Delete).
    fn kill_word_forward(&self) {
        self.push_undo("kill");
        let killed = if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            let col = state.cursor_col;
            // Kill only the current word (not the trailing delimiter) —
            // `find_word_end` stops at the last word char.
            let end = crate::word_navigation::find_word_end(&state.lines[row], col);
            let dead: String = state.lines[row].drain(col..end).collect();
            dead
        } else {
            String::new()
        };
        self.kill(killed, false);
        self.notify_change();
    }

    /// Jump to the previous word boundary (Alt+Left).
    fn cursor_word_left(&self) {
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            state.cursor_col =
                crate::word_navigation::find_word_backward(&state.lines[row], state.cursor_col);
        }
    }

    /// Jump to the next word boundary (Alt+Right).
    fn cursor_word_right(&self) {
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            state.cursor_col =
                crate::word_navigation::find_word_forward(&state.lines[row], state.cursor_col);
        }
    }

    /// Page-scroll the editor caret up/down by a page (pi `tui.editor.pageUp/
    /// pageDown`): move the cursor row by a page and clamp the column to the
    /// target line (page = 10 rows; a terminal-height-aware version would need
    /// the viewport, which the component doesn't own).
    fn page_scroll(&self, dir: i32) {
        const PAGE: usize = 10;
        if let Ok(mut state) = self.state.lock() {
            let target = if dir > 0 {
                (state.cursor_row + PAGE).min(state.lines.len().saturating_sub(1))
            } else {
                state.cursor_row.saturating_sub(PAGE)
            };
            state.cursor_row = target;
            let max = state.lines[target].len();
            state.cursor_col = snap_boundary(&state.lines[target], state.cursor_col.min(max));
        }
    }

    /// Jump the caret to the next/previous occurrence of `target` (pi
    /// `jumpToChar`): scan the current line from after/before the caret, then
    /// onward/backward line by line. Byte-safe: `find`/`rfind` return char
    /// boundaries and all search offsets start on one.
    fn jump_to_char(&self, target: char, dir: i32) {
        let forward = dir > 0;
        let mut state = self.state.lock().unwrap();
        let start_row = state.cursor_row;
        let mut row = start_row;
        let step: isize = if forward { 1 } else { -1 };
        loop {
            let line = &state.lines[row];
            let search_from = if row == start_row {
                if forward {
                    (state.cursor_col + target.len_utf8()).min(line.len())
                } else {
                    state.cursor_col.saturating_sub(target.len_utf8())
                }
            } else if forward {
                0
            } else {
                line.len()
            };
            let found = if forward {
                line[search_from..].find(target).map(|i| search_from + i)
            } else {
                line[..search_from].rfind(target)
            };
            if let Some(idx) = found {
                state.cursor_row = row;
                state.cursor_col = idx;
                return;
            }
            let next = row as isize + step;
            if next < 0 || next >= state.lines.len() as isize {
                return;
            }
            row = next as usize;
        }
    }

    /// Set an explicit top/bottom border color. `None` restores the theme's
    /// default border color. Mirrors upstream's `editor.borderColor`.
    pub fn set_border_color(&self, color: Option<Color>) {
        if let Ok(mut guard) = self.border_color.lock() {
            *guard = color;
        }
    }

    /// The current explicit border color, if any.
    pub fn border_color(&self) -> Option<Color> {
        self.border_color.lock().ok().and_then(|guard| *guard)
    }

    /// Set the working state. When Some, shows a spinner + elapsed time + message
    /// inside the top border. When None, shows the normal border.
    pub fn set_working(&self, working: Option<WorkingState>) {
        if let Ok(mut guard) = self.working.lock() {
            *guard = working;
        }
    }

    /// Get the current working state, if any.
    pub fn working(&self) -> Option<WorkingState> {
        self.working.lock().ok().and_then(|guard| guard.clone())
    }

    /// Submit current text (Enter).
    fn submit(&self) {
        // Send the EXPANDED draft: a folded `[paste #id …]` marker must reach
        // the model as the user's real content, not the placeholder.
        let text = self.get_expanded_text();
        if let Ok(mut state) = self.state.lock() {
            if !text.is_empty() {
                state.history.push(text.clone());
                state.history_index = state.history.len();
            }
        }
        // Submitted text is committed — don't let Ctrl+- resurrect it.
        self.undo_stack.lock().unwrap().clear();
        self.redo_stack.lock().unwrap().clear();
        *self.last_action.lock().unwrap() = None;
        if let Ok(cb) = self.on_submit.lock() {
            if let Some(callback) = cb.as_ref() {
                callback(&text);
            }
        }
        self.clear();
    }

    /// Notify change callback.
    fn notify_change(&self) {
        self.paste_revision.fetch_add(1, Ordering::SeqCst);
        if let Ok(cb) = self.on_change.lock() {
            if let Some(callback) = cb.as_ref() {
                let text = self.get_text();
                callback(&text);
            }
        }
    }

    /// Handle keyboard input.
    pub fn handle_key(&self, key: crossterm::event::KeyEvent) -> bool {
        self.paste_revision.fetch_add(1, Ordering::SeqCst);
        match (key.modifiers, key.code) {
            // Navigation — plain movement clears any active selection;
            // Shift+movement extends it.
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.clear_selection();
                self.cursor_left();
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.clear_selection();
                self.cursor_right();
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.clear_selection();
                self.cursor_up();
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.clear_selection();
                self.cursor_down();
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.clear_selection();
                self.cursor_home();
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                self.clear_selection();
                self.cursor_end();
            }
            (KeyModifiers::SHIFT, KeyCode::Left) => {
                self.begin_selection();
                self.cursor_left();
            }
            (KeyModifiers::SHIFT, KeyCode::Right) => {
                self.begin_selection();
                self.cursor_right();
            }
            (KeyModifiers::SHIFT, KeyCode::Up) => {
                self.begin_selection();
                self.cursor_up();
            }
            (KeyModifiers::SHIFT, KeyCode::Down) => {
                self.begin_selection();
                self.cursor_down();
            }
            (KeyModifiers::SHIFT, KeyCode::Home) => {
                self.begin_selection();
                self.cursor_home();
            }
            (KeyModifiers::SHIFT, KeyCode::End) => {
                self.begin_selection();
                self.cursor_end();
            }

            // Editing
            (KeyModifiers::NONE, KeyCode::Backspace) => self.backspace(),
            (KeyModifiers::NONE, KeyCode::Delete) => self.delete(),
            (KeyModifiers::NONE, KeyCode::Enter)
            | (KeyModifiers::NONE, KeyCode::Char('\n'))
            | (KeyModifiers::NONE, KeyCode::Char('\r')) => {
                // Submit on Enter (mirrors TS `tui.input.submit`). The
                // `Char('\n')`/`Char('\r')` forms cover a client whose terminal
                // hands us the codepoint directly (e.g. the kitty keyboard
                // protocol's CSI-u path); they used to fall through to the
                // generic char arm and insert a literal newline.
                self.submit();
            }
            // Shift+Enter inserts a newline (mirrors `tui.input.newLine`).
            (KeyModifiers::SHIFT, KeyCode::Enter)
            | (KeyModifiers::SHIFT, KeyCode::Char('\r'))
            | (KeyModifiers::SHIFT, KeyCode::Char('\n')) => self.insert("\n"),
            // A host terminal that rewrites Shift+Enter delivers Ctrl+Enter
            // instead of a SHIFT-modified Enter: Windows Terminal's
            // `sendInput "\n"` binding turns the chord into a bare LF, which
            // ConPTY hands back as `Enter + CONTROL` (the LF itself is
            // indistinguishable from Ctrl+J once raw mode is on). #108's
            // native-Shift guard only fires on `Enter` with empty modifiers,
            // so this shape used to fall through to `_ => return false` and
            // drop the key silently. Bind it to the same newline as Ctrl+J.
            (KeyModifiers::CONTROL, KeyCode::Enter)
            | (KeyModifiers::CONTROL, KeyCode::Char('\r'))
            | (KeyModifiers::CONTROL, KeyCode::Char('\n')) => self.insert_no_undo("\n"),

            // Ctrl shortcuts
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => self.cursor_home(),
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => self.cursor_end(),
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                // Kill to end of line (push the killed text onto the ring).
                self.push_undo("kill");
                let killed = if let Ok(mut state) = self.state.lock() {
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    let dead: String = state.lines[row].drain(col..).collect();
                    state.lines[row].truncate(col);
                    dead
                } else {
                    String::new()
                };
                self.kill(killed, false);
                self.notify_change();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                // Kill to start of line (killed text prepends to the ring entry
                // so Ctrl+Y pastes it back in the same order).
                self.push_undo("kill");
                let killed = if let Ok(mut state) = self.state.lock() {
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    let before: String = state.lines[row].drain(..col).collect();
                    state.cursor_col = 0;
                    before
                } else {
                    String::new()
                };
                self.kill(killed, true);
                self.notify_change();
            }
            // Undo / redo (pi binds undo to Ctrl+-; Ctrl+R redo is a Rust port
            // convenience since pi has no redo binding).
            (KeyModifiers::CONTROL, KeyCode::Char('-')) => self.undo(),
            (KeyModifiers::CONTROL, KeyCode::Char('r')) => self.redo(),
            // Selection cut/copy/paste (Ctrl+X / Ctrl+C / Ctrl+V).
            (KeyModifiers::CONTROL, KeyCode::Char('x')) => {
                self.cut_selection();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.copy_selection();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('v')) => {
                self.yank();
            }
            // Kill-ring yank (Ctrl+Y) / yank-pop (Alt+Y).
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => self.yank(),
            (KeyModifiers::ALT, KeyCode::Char('y')) => self.yank_pop(),
            // Delete the word before the cursor, killing it (Alt+Backspace).
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => self.kill_word_backward(),
            (KeyModifiers::ALT, KeyCode::Backspace) => self.kill_word_backward(),
            (KeyModifiers::ALT, KeyCode::Char('d')) | (KeyModifiers::ALT, KeyCode::Delete) => {
                self.kill_word_forward();
            }
            // Word navigation (Alt+Left / Alt+Right — pi's word-jump bindings).
            (KeyModifiers::ALT, KeyCode::Left) => self.cursor_word_left(),
            (KeyModifiers::ALT, KeyCode::Right) => self.cursor_word_right(),
            // Ctrl+J inserts a newline (pi tui.input.newLine alongside
            // Shift+Enter). Ctrl+D deletes the char forward (deleteCharForward).
            (KeyModifiers::CONTROL, KeyCode::Char('j')) => self.insert_no_undo(
                "
",
            ),
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => self.delete(),
            // Editor page scroll (pi tui.editor.pageUp/pageDown — the
            // unmodified PageUp/Down are the alt-screen transcript scroll).
            (KeyModifiers::CONTROL, KeyCode::PageUp) => self.page_scroll(-1),
            (KeyModifiers::CONTROL, KeyCode::PageDown) => self.page_scroll(1),
            // Character jump mode (pi jumpForward/jumpBackward): Ctrl+] jumps
            // forward to the next typed char, Ctrl+Alt+] backward. NB: a
            // CONTROL|ALT pattern in a match arm is an or-pattern (matches
            // EITHER modifier), so the backward arm uses a guard.
            (_, KeyCode::Char(']')) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+Alt+] jumps backward, Ctrl+] forward. A bare CONTROL
                // arm wouldn't match the CONTROL|ALT combination.
                if key.modifiers.contains(KeyModifiers::ALT) {
                    *self.jump_mode.lock().unwrap() = Some(-1);
                } else {
                    *self.jump_mode.lock().unwrap() = Some(1);
                }
            }

            // Regular character input — unless a character jump is pending
            // (Ctrl+] / Ctrl+Alt+] consumed the key as the jump target).
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                let jump = self.jump_mode.lock().unwrap().take();
                match jump {
                    Some(dir) => self.jump_to_char(c, dir),
                    None => self.insert(&c.to_string()),
                }
            }

            _ => return false,
        }
        true
    }
}

impl Component for Editor {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self.render_snapshot();
        let mut content_lines = Vec::new();

        // Keep the cursor marker while working: layout uses it to keep the
        // draft caret visible, and the terminal uses it for IME positioning.
        let pad = " ".repeat(self.options.padding_x);
        // `prompt` is no longer rendered (mirrors the TS editor, which draws a
        // bordered box with padding-only lines and no `> ` prefix — see
        // `editor.ts:539-578`). The `EditorStyle.prompt` field is retained for
        // API compatibility but unread here.
        let content_width = width.saturating_sub(self.options.padding_x).max(1);
        // Wrapping uses one width for every logical row, the cursor's row
        // included. Reserving the cursor cell per-row used to make the wrap
        // point depend on where the caret happened to be: the same text
        // reflowed one column earlier while it was being edited and again
        // after the caret moved away. Upstream keeps a single `layoutWidth`
        // for the same reason (`editor.ts:489`). The one cell the caret needs
        // is taken from layout width once, here, so it is spent identically
        // whether or not the row carries the caret.
        let layout_width = content_width.saturating_sub(1).max(1);
        for (row_idx, line) in state.lines.iter().enumerate() {
            let ranges = wrap_line_ranges(line, layout_width);
            for (chunk_start, chunk_end) in ranges {
                let chunk = &line[chunk_start..chunk_end];
                // Placeholder (dim) only on the single empty line at the cursor —
                // honors callers that set `placeholder`; production drops it.
                let content =
                    if chunk.is_empty() && row_idx == state.cursor_row && state.lines.len() == 1 {
                        if let Some(placeholder) = &self.options.placeholder {
                            format!("\x1b[2m{}\x1b[22m", placeholder)
                        } else {
                            String::new()
                        }
                    } else {
                        chunk.to_string()
                    };

                // Selection highlight: inverse-video the span on this row (byte
                // offsets are char boundaries — both anchor and caret always sit
                // on one).
                let mut content = content;
                if let Some((sc, ec)) = self.selection_span_on_row(&state, row_idx) {
                    let local_start = sc.max(chunk_start).min(chunk_end);
                    let local_end = ec.max(chunk_start).min(chunk_end);
                    if local_start < local_end {
                        let before = &line[chunk_start..local_start];
                        let sel = &line[local_start..local_end];
                        let after = &line[local_end..chunk_end];
                        content = format!("{before}\x1b[7m{sel}\x1b[27m{after}");
                    }
                }

                let visible_w = crate::ansi::visible_width(&content);
                // Pad the line out to the full width (left padding + content +
                // right padding) so the border box has a uniform interior width
                // (mirrors editor.ts:522-578). The caret's cell is reserved by
                // `layout_width`, so every row — caret row included — pads to
                // the same right edge; trimming this row instead would leave a
                // one-column notch that moves as the caret moves.
                let right_pad = " ".repeat(
                    width
                        .saturating_sub(self.options.padding_x)
                        .saturating_sub(visible_w),
                );

                let mut rendered = format!("{pad}{content}{right_pad}");

                // Cursor marker if this focused row holds the cursor. `cursor_col`
                // is a byte offset into `line`; the rendered prefix up to the caret
                // is `pad` (ASCII, `padding_x` bytes) + `line[..cursor_col]`.
                // Because `content` may carry ANSI escapes (placeholder branch),
                // and the prefix length is computed in *bytes* of the source line,
                // snap the insertion point to the nearest preceding char boundary
                // in `rendered` before byte-slicing — never slice mid-character.
                if state.focused
                    && row_idx == state.cursor_row
                    && state.cursor_col >= chunk_start
                    && state.cursor_col <= chunk_end
                {
                    let cursor_col = UnicodeWidthStr::width(
                        line[chunk_start..state.cursor_col.min(chunk_end)]
                            .to_string()
                            .as_str(),
                    );
                    rendered = insert_cursor_marker(&rendered, self.options.padding_x + cursor_col);
                }

                content_lines.push(rendered);
            }
        }

        // Guard against an empty `lines` vector (the invariant is `[""]`, but
        // be defensive): emit one blank interior row so the box still renders.
        if content_lines.is_empty() {
            content_lines.push(format!(
                "{pad}{}",
                " ".repeat(width.saturating_sub(self.options.padding_x))
            ));
        }

        // Full-width top + bottom border, colored via the theme border color
        // (mirrors editor.ts:494,530,587). The global `theme()` is read-only
        // after OnceLock init; `Color::fg` wraps the `─` run in the escape.
        // An explicit `border_color` (bash mode) wins over the theme default.
        let border_color = self
            .border_color
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .unwrap_or(crate::theme::theme().colors.border);
        // Check if we have a working state to display in the top border.
        //
        // The frame is derived from elapsed wall-clock time (one step per
        // `SPINNER_FRAME_MS`), mirroring upstream's fixed-cadence `setInterval`
        // animation. It used to advance once per render, which tied the visible
        // speed to the repaint rate: streamed output scheduled a repaint per
        // token and spun the spinner at the 16ms render-throttle rate.
        let working_state = self.working.lock().ok().and_then(|guard| guard.clone());
        let top_border = if let Some(ref working) = working_state {
            // Working indicator embedded in the top border: spinner + message +
            // elapsed time, with the remaining width filled by border glyphs.
            // The frame set and cadence are shared with `Loader` so the two
            // never drift.
            let frames = crate::loader::SPINNER_FRAMES;
            let spinner_char =
                frames[crate::loader::Loader::frame_at(working.started_at.elapsed(), frames.len())];
            let colors = crate::theme::theme().colors;

            let elapsed = working.started_at.elapsed();
            let elapsed_str = if elapsed.as_secs() >= 60 {
                format!("{}m{}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
            } else {
                format!("{}s", elapsed.as_secs())
            };

            let plain = format!(" {} {} {} ", spinner_char, working.message, elapsed_str);
            let indicator_width = crate::ansi::visible_width(&plain);

            if width > indicator_width {
                let colored = format!(
                    " {} {} {} ",
                    colors.accent.fg(&spinner_char.to_string()),
                    colors.muted.fg(&working.message),
                    colors.muted.fg(&elapsed_str),
                );
                let right = border_color.fg(&"─".repeat(width - indicator_width - 1));
                format!("{}{}{}", border_color.fg("─"), colored, right)
            } else {
                border_color.fg(&"─".repeat(width))
            }
        } else {
            border_color.fg(&"─".repeat(width))
        };

        let bottom_border = {
            let horizontal = "─".repeat(width);
            border_color.fg(&horizontal)
        };

        let mut lines = Vec::with_capacity(content_lines.len() + 2);
        lines.push(top_border);
        lines.extend(content_lines);
        lines.push(bottom_border);
        lines
    }

    fn invalidate(&self) {
        // Editor doesn't have external cache
    }

    fn handle_input(&self, _data: &str) -> bool {
        // Parse key event from crossterm format
        // This is a simplified version - in real usage, you'd receive KeyEvent directly
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Focusable for Editor {
    fn set_focused(&self, focused: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.focused = focused;
        }
    }

    fn is_focused(&self) -> bool {
        self.state.lock().map(|s| s.focused).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_snapshot_is_independent_of_submitted_history() {
        let editor = Editor::simple();
        editor.set_text("读 draft\nsecond line");
        editor.set_focused(true);
        editor.set_cursor(0, 3);
        editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::SHIFT,
        ));
        let snapshot = editor.render_snapshot();
        let rendered = editor.render(12);
        assert!(rendered.iter().any(|line| line.contains("\x1b[7m")));
        assert!(rendered.iter().any(|line| line.contains(CURSOR_MARKER)));

        // Grow retained history to 16 MiB without changing any paint input.
        // Comparing the entire snapshot is deterministic, not a timing gate.
        {
            let mut state = editor.state.lock().unwrap();
            state.history = vec!["h".repeat(4096); 4096];
            state.history_index = state.history.len();
        }
        for width in [12, 40] {
            let before = editor.render_snapshot();
            let _ = editor.render(width);
            assert_eq!(before, snapshot);
            assert_eq!(editor.render_snapshot(), snapshot);
        }
        assert_eq!(editor.render(12), rendered);
        {
            let state = editor.state.try_lock().expect("snapshot releases mutex");
            assert_eq!(state.history.len(), 4096);
            assert_eq!(state.history_index, 4096);
            assert!(state
                .history
                .iter()
                .all(|entry| entry.len() == 4096 && entry.bytes().all(|byte| byte == b'h')));
        }

        // The snapshot owns its draft, independently of later editor changes.
        editor.set_text("changed");
        assert_eq!(snapshot.lines, ["读 draft", "second line"]);
        assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (1, 3));
        assert_eq!(snapshot.selection_anchor, Some((0, 3)));
        assert!(snapshot.focused);
    }

    #[test]
    fn rendering_retains_submissions_and_draft_navigation() {
        let editor = Editor::simple();
        for prompt in ["first submitted prompt", "second submitted prompt"] {
            editor.set_text(prompt);
            editor.submit();
            let _ = editor.render(20);
        }
        editor.set_text("first line\nsecond line");
        editor.set_cursor(1, 4);
        let _ = editor.render(10);
        editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        ));
        assert_eq!(editor.cursor_position(), (0, 4));
        let _ = editor.render(40);
        editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        assert_eq!(editor.cursor_position(), (1, 4));
        assert_eq!(editor.get_text(), "first line\nsecond line");
        let state = editor.state.lock().unwrap();
        assert_eq!(
            state.history,
            ["first submitted prompt", "second submitted prompt"]
        );
        assert_eq!(state.history_index, 2);
    }

    #[test]
    fn working_spinner_follows_wall_clock_cadence() {
        use crate::component::Component;
        let editor = Editor::simple();
        editor.set_working(Some(WorkingState {
            // 5ms into the first window: ~75ms of scheduling slack before the
            // two repaints could legally land in different windows.
            started_at: std::time::Instant::now() - std::time::Duration::from_millis(5),
            message: "Working…".to_string(),
        }));

        // The indicator lives in the top border and reports the message.
        let first = crate::strip_ansi(&editor.render(60)[0]);
        assert!(first.contains("Working"), "top border: {first}");

        // The frame comes from elapsed time, not from the repaint count: two
        // renders inside the same 80ms window show the same glyph. Under the
        // previous per-render stepping these two renders differed, which is
        // what made streaming (one repaint per token) spin at the render
        // throttle's rate.
        let a = crate::strip_ansi(&editor.render(60)[0]);
        let b = crate::strip_ansi(&editor.render(60)[0]);
        assert_eq!(
            a, b,
            "repaints within one frame window must not move the spinner"
        );

        // Elapsed time does move it: a state started ~125ms ago (one step past
        // the 80ms boundary) shows the second frame.
        editor.set_working(Some(WorkingState {
            started_at: std::time::Instant::now() - std::time::Duration::from_millis(125),
            message: "Working…".to_string(),
        }));
        let later = crate::strip_ansi(&editor.render(60)[0]);
        assert_ne!(
            a, later,
            "crossing an 80ms boundary must advance the spinner"
        );

        // Idle editors have no indicator.
        editor.set_working(None);
        let idle = crate::strip_ansi(&editor.render(60)[0]);
        assert!(!idle.contains("Working"), "idle border: {idle}");
    }

    #[test]
    fn working_spinner_uses_native_pi_frames_and_cadence() {
        // The editor used to hand-copy the frame list and drifted (`⠘` where
        // upstream has `⠙`), so the two indicators animated differently. Both
        // now share `SPINNER_FRAMES`.
        assert_eq!(crate::loader::SPINNER_FRAMES.len(), 10);
        assert_eq!(
            crate::loader::SPINNER_FRAMES.iter().collect::<String>(),
            "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
        );
        // 10 frames at 80ms = the 800ms full cycle upstream animates at.
        assert_eq!(crate::loader::SPINNER_FRAME_MS, 80);

        // The rendered indicator steps through exactly that list, in order,
        // one frame per SPINNER_FRAME_MS of elapsed wall clock — upstream's
        // `setInterval` cadence, evaluated at render time.
        use crate::component::Component;
        let editor = Editor::simple();
        let spinner_of = |line: &str| {
            let plain = crate::strip_ansi(line);
            crate::loader::SPINNER_FRAMES
                .iter()
                .copied()
                .find(|c| plain.contains(*c))
        };
        let mut seen = Vec::new();
        for i in 0..10u32 {
            editor.set_working(Some(WorkingState {
                started_at: std::time::Instant::now()
                    - std::time::Duration::from_millis((i * 80 + 10).into()),
                message: "Working…".to_string(),
            }));
            seen.push(spinner_of(&editor.render(60)[0]).expect("a spinner glyph"));
        }
        let expected: Vec<char> = crate::loader::SPINNER_FRAMES.to_vec();
        assert_eq!(seen, expected);
    }

    #[test]
    fn loader_frame_at_follows_elapsed_wall_clock() {
        use std::time::Duration;
        // Mid-window stays on the frame; each 80ms boundary steps once; the
        // cycle wraps after 10 frames (800ms).
        assert_eq!(
            crate::loader::Loader::frame_at(Duration::from_millis(0), 10),
            0
        );
        assert_eq!(
            crate::loader::Loader::frame_at(Duration::from_millis(79), 10),
            0
        );
        assert_eq!(
            crate::loader::Loader::frame_at(Duration::from_millis(80), 10),
            1
        );
        assert_eq!(
            crate::loader::Loader::frame_at(Duration::from_millis(750), 10),
            9
        );
        assert_eq!(
            crate::loader::Loader::frame_at(Duration::from_millis(800), 10),
            0
        );
        // A frame count of 1 (or 0 from a bad caller) must not divide by zero.
        assert_eq!(
            crate::loader::Loader::frame_at(Duration::from_millis(999), 1),
            0
        );
        assert_eq!(
            crate::loader::Loader::frame_at(Duration::from_millis(999), 0),
            0
        );
    }

    #[test]
    fn idle_cadence_matches_native_pi() {
        // One frame per `SPINNER_FRAME_MS` of wall clock, matching upstream's
        // `setInterval` cadence. The render tick only *requests a repaint*;
        // the visible frame is derived from elapsed time, so a repaint storm
        // (streamed output) cannot speed the spinner up and a quiet stretch
        // cannot freeze it.
        assert_eq!(crate::loader::SPINNER_FRAME_MS, 80);
        assert_eq!(
            crate::loader::SPINNER_FRAME_MS * crate::loader::SPINNER_FRAMES.len() as u64,
            800,
            "a full cycle should take 800ms, like upstream"
        );
    }

    #[test]
    fn working_caret_preserves_draft_text_and_input_position() {
        for (draft, column) in [("abcdef", 2), ("中文草稿", "中文".len())] {
            for width in [12, 40] {
                let editor = Editor::simple();
                editor.set_focused(true);
                editor.set_text(draft);
                editor.set_cursor(0, column);
                let idle = editor.render(width);
                let position = crate::extract_cursor_position(&idle, idle.len());
                assert!(position.is_some());
                let content = crate::strip_ansi(&idle[1].replace(CURSOR_MARKER, ""));
                assert_eq!(content.trim_end(), draft);

                editor.set_working(Some(WorkingState {
                    started_at: std::time::Instant::now(),
                    message: "Working".to_string(),
                }));
                for _ in 0..2 {
                    let working = editor.render(width);
                    assert_eq!(
                        crate::extract_cursor_position(&working, working.len()),
                        position,
                        "working frames must retain the layout and IME cursor position"
                    );
                    assert_eq!(
                        crate::strip_ansi(&working[1].replace(CURSOR_MARKER, "")),
                        content,
                        "drawing the working caret must not shift the draft"
                    );
                }
                editor.set_working(None);
                let idle_again = editor.render(width);
                assert_eq!(
                    crate::extract_cursor_position(&idle_again, idle_again.len()),
                    position
                );
                assert_eq!(editor.get_text(), draft);
                assert_eq!(editor.cursor_position(), (0, column));

                editor.set_focused(false);
                assert!(crate::extract_cursor_position(&editor.render(width), 20).is_none());
            }
        }
    }

    #[test]
    fn working_caret_keeps_the_last_draft_line_in_a_constrained_viewport() {
        let editor = Arc::new(Editor::simple());
        editor.set_focused(true);
        editor.set_text("line0\nline1\nline2\nline3\nline4\nline5\nline6\nline7");
        editor.set_cursor(7, "line7".len());
        let idle = crate::render_layout_frame(editor.clone(), 20, 4);
        assert!(idle.root.line_offset > 0);
        assert!(idle.lines.iter().any(|line| line.contains("line7")));
        let position = crate::extract_cursor_position(&idle.lines, idle.height);
        assert!(position.is_some());

        editor.set_working(Some(WorkingState {
            started_at: std::time::Instant::now(),
            message: "Working".to_string(),
        }));
        for _ in 0..2 {
            let working = crate::render_layout_frame(editor.clone(), 20, 4);
            assert_eq!(working.root.line_offset, idle.root.line_offset);
            assert!(working.lines.iter().any(|line| line.contains("line7")));
            assert_eq!(
                crate::extract_cursor_position(&working.lines, working.height),
                position
            );
        }
        editor.set_working(None);
        let idle_again = crate::render_layout_frame(editor.clone(), 20, 4);
        assert_eq!(idle_again.root.line_offset, idle.root.line_offset);
        assert_eq!(
            crate::extract_cursor_position(&idle_again.lines, idle_again.height),
            position
        );
    }

    #[test]
    fn explicit_border_color_overrides_the_theme_border() {
        use crate::component::Component;
        let editor = Editor::simple();
        editor.set_focused(true);
        editor.insert("echo hi");

        let default_border = crate::theme::theme().colors.border.fg(&"─".repeat(20));
        assert!(editor.render(20).contains(&default_border));
        assert_eq!(editor.border_color(), None);

        // Bash mode sets an explicit accent; the border line changes and the
        // theme default is gone.
        editor.set_border_color(Some(crate::theme::theme().colors.bash_mode));
        let bash_border = crate::theme::theme().colors.bash_mode.fg(&"─".repeat(20));
        assert_eq!(
            editor.border_color(),
            Some(crate::theme::theme().colors.bash_mode)
        );
        let rendered = editor.render(20);
        assert!(rendered.contains(&bash_border), "{rendered:?}");
        assert!(!rendered.contains(&default_border));

        // Clearing restores the theme default (what `updateEditorBorderColor`
        // does once the draft no longer starts with `!`).
        editor.set_border_color(None);
        assert!(editor.render(20).contains(&default_border));
    }

    #[test]
    fn clear_and_set_text_notify_change_observers() {
        // Upstream's `setTextInternal` fires onChange for programmatic edits;
        // the bash-mode border and autocomplete rely on it to drop stale state.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_cb = calls.clone();
        let editor = Editor::simple();
        editor.on_change(Arc::new(move |_text: &str| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
        }));

        editor.set_text("!ls");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        editor.clear();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn snap_boundary_keeps_char_boundaries_including_end() {
        // The regression: a caret placed exactly at the END of the text (what
        // autocomplete accept does — `set_cursor(0, text.len())`) used to snap
        // one char EARLY, rendering "/mode|l" instead of "/model|".
        assert_eq!(snap_boundary("/mo", 3), 3);
        assert_eq!(snap_boundary("/model", 6), 6);
        assert_eq!(snap_boundary("hello", 5), 5);
        // Interior boundaries are kept as-is.
        assert_eq!(snap_boundary("/model", 0), 0);
        assert_eq!(snap_boundary("/model", 2), 2);
        // Out-of-range clamps to the end (a valid boundary).
        assert_eq!(snap_boundary("/mo", 99), 3);
    }

    #[test]
    fn snap_boundary_mid_multibyte_snaps_down_to_char_start() {
        // "你" occupies bytes 0..3: a position inside it must snap to 0.
        assert_eq!(snap_boundary("你", 1), 0);
        assert_eq!(snap_boundary("你", 2), 0);
        // Position after it (end of string) stays 3.
        assert_eq!(snap_boundary("你", 3), 3);
        // Mixed: "a你b" — 你 is bytes 1..4; end of string is 5.
        assert_eq!(snap_boundary("a你b", 5), 5);
        assert_eq!(snap_boundary("a你b", 2), 1);
    }

    #[test]
    fn left_arrow_keeps_text_in_render() {
        use crate::component::Component;
        use crossterm::event::{KeyCode, KeyModifiers};
        let mk = |m, c| crossterm::event::KeyEvent::new(c, m);
        let editor = Editor::simple();
        editor.set_focused(true);
        editor.insert("hello world");
        editor.handle_key(mk(KeyModifiers::NONE, KeyCode::Left));
        let rendered = editor.render(40).join(
            "
",
        );
        assert!(
            rendered.contains("hello"),
            "text must survive Left, got: {rendered:?}"
        );
    }
    #[test]
    fn test_char_jump_and_page_scroll() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mk = |m, c| crossterm::event::KeyEvent::new(c, m);
        let editor = Editor::simple();

        // Character jump: from the end, Ctrl+Alt+] then 'x' jumps backward to
        // the previous 'x' ("alpha xray xray" — the second "xray" starts at 11).
        editor.insert("alpha xray xray");
        editor.handle_key(mk(KeyModifiers::CONTROL | KeyModifiers::ALT, KeyCode::End)); // caret at end
        editor.handle_key(mk(
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyCode::Char(']'),
        ));
        // The next printable char is consumed as the jump target.
        editor.handle_key(mk(KeyModifiers::NONE, KeyCode::Char('x')));
        assert_eq!(editor.cursor_position(), (0, 11));

        // Page scroll on a multi-line buffer clamps to the last row.
        editor.set_text(
            "l0
l1
l2
l3
l4
l5
l6
l7
l8
l9
l10
l11
l12",
        );
        editor.set_cursor(0, 2);
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::PageDown));
        let (row, _) = editor.cursor_position();
        assert!(row >= 10, "page-down moves ~10 rows, got {row}");
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::PageUp));
        let (row2, _) = editor.cursor_position();
        assert!(row2 <= 2, "page-up returns near the start, got {row2}");
    }
    #[test]
    fn test_word_ops_and_new_keys() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mk = |m, c| crossterm::event::KeyEvent::new(c, m);
        let editor = Editor::simple();
        editor.insert("alpha beta gamma");

        // Ctrl+W kills the word before the cursor.
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::End));
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::Char('w')));
        assert_eq!(editor.get_text(), "alpha beta ");

        // Alt+Left jumps back a word; Alt+D kills the word after.
        editor.handle_key(mk(KeyModifiers::ALT, KeyCode::Left));
        assert_eq!(editor.cursor_position(), (0, 6));
        editor.handle_key(mk(KeyModifiers::ALT, KeyCode::Char('d')));
        assert_eq!(editor.get_text(), "alpha  ");

        // Ctrl+J inserts a newline.
        editor.set_text("a");
        editor.set_cursor(0, 1);
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::Char('j')));
        assert_eq!(
            editor.get_text(),
            "a
"
        );

        // Ctrl+D deletes the char forward (deleteCharForward).
        editor.set_text("abc");
        editor.set_cursor(0, 1);
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::Char('d')));
        assert_eq!(editor.get_text(), "ac");
    }
    #[test]
    fn test_selection_cut_copy_paste() {
        let editor = Editor::simple();
        editor.insert("hello world");
        // Caret to the start, then Shift+Right ×5 selects "hello".
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Home,
            crossterm::event::KeyModifiers::NONE,
        ));
        let shift_right = || {
            editor.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::SHIFT,
            ))
        };
        for _ in 0..5 {
            shift_right();
        }
        assert!(editor.has_selection());
        assert_eq!(editor.selected_text().as_deref(), Some("hello"));
        // Ctrl+C copies (selection intact), then plain Right clears it.
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(editor.get_text(), "hello world");
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!editor.has_selection(), "plain movement clears selection");
        // Move to the end and yank the copied text.
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::End,
            crossterm::event::KeyModifiers::NONE,
        ));
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('v'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(editor.get_text(), "hello worldhello");
    }

    #[test]
    fn test_selection_replace_and_cut() {
        let editor = Editor::simple();
        editor.insert("abcdef");
        // Caret to the start, then Shift+Right ×4 selects "abcd".
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Home,
            crossterm::event::KeyModifiers::NONE,
        ));
        for _ in 0..4 {
            editor.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::SHIFT,
            ));
        }
        // Backspace deletes the selection ("abcd" → "ef").
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Backspace,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(editor.get_text(), "ef", "backspace deletes the selection");
        // Undo restores it.
        editor.undo();
        assert_eq!(editor.get_text(), "abcdef");
        // Cut: return to the start, select "ab", Ctrl+X removes + kills it.
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Home,
            crossterm::event::KeyModifiers::NONE,
        ));
        for _ in 0..2 {
            editor.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::SHIFT,
            ));
        }
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(editor.get_text(), "cdef");
        // Type over a selection replaces it.
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::SHIFT,
        ));
        editor.insert("Z");
        assert_eq!(editor.get_text(), "Zdef", "typing replaces the selection");
    }
    #[test]
    fn test_undo_redo_roundtrip() {
        let editor = Editor::simple();
        editor.insert("hello");
        editor.insert(" world");
        // Consecutive inserts coalesce: one undo restores the empty state.
        assert_eq!(editor.get_text(), "hello world");
        editor.undo();
        assert_eq!(editor.get_text(), "", "coalesced inserts undo as one step");
        // Redo restores everything.
        editor.redo();
        assert_eq!(editor.get_text(), "hello world");
        // A fresh edit after undo clears redo.
        editor.undo();
        editor.insert("hi");
        editor.redo();
        assert_eq!(editor.get_text(), "hi", "new edit clears the redo stack");
    }

    #[test]
    fn test_kill_ring_yank() {
        let editor = Editor::simple();
        editor.insert("alpha beta gamma");
        // Ctrl+K kills to end of line.
        editor.set_cursor(0, 6); // after "alpha "
        let key = |m, c| crossterm::event::KeyEvent::new(c, m);
        editor.handle_key(key(
            crossterm::event::KeyModifiers::CONTROL,
            crossterm::event::KeyCode::Char('k'),
        ));
        assert_eq!(editor.get_text(), "alpha ");
        // Ctrl+Y yanks it back.
        editor.handle_key(key(
            crossterm::event::KeyModifiers::CONTROL,
            crossterm::event::KeyCode::Char('y'),
        ));
        assert_eq!(editor.get_text(), "alpha beta gamma");
    }

    #[test]
    fn test_undo_after_kill() {
        let editor = Editor::simple();
        editor.insert("hello world");
        editor.set_cursor(0, 5);
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('k'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(editor.get_text(), "hello");
        editor.undo();
        assert_eq!(editor.get_text(), "hello world", "kill is undoable");
    }

    #[test]
    fn shift_enter_encodings_insert_newline_without_submitting() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for code in [KeyCode::Enter, KeyCode::Char('\r'), KeyCode::Char('\n')] {
            let editor = Editor::simple();
            editor.on_submit(Arc::new(|_| panic!("Shift+Enter must not submit")));
            editor.insert("draft");
            editor.handle_key(KeyEvent::new(code, KeyModifiers::SHIFT));
            assert_eq!(editor.get_text(), "draft\n");
        }
    }

    /// Regression: a host terminal can rewrite Shift+Enter before rpi ever sees
    /// it. Windows Terminal's `sendInput "\n"` binding emits a bare LF, which
    /// ConPTY delivers as `Enter + CONTROL` — the same record a real Ctrl+Enter
    /// produces. #108's guard only covers `Enter` with empty modifiers, so this
    /// shape reached the catch-all arm and the key was dropped with no feedback.
    /// It must insert a newline and must not submit.
    #[test]
    fn ctrl_enter_encodings_insert_newline_without_submitting() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for code in [KeyCode::Enter, KeyCode::Char('\r'), KeyCode::Char('\n')] {
            let editor = Editor::simple();
            editor.on_submit(Arc::new(|_| {
                panic!("rewritten Shift+Enter must not submit")
            }));
            editor.insert("draft");
            editor.handle_key(KeyEvent::new(code, KeyModifiers::CONTROL));
            assert_eq!(editor.get_text(), "draft\n");
        }
    }

    #[test]
    fn test_submit_clears_undo() {
        let editor = Editor::simple();
        editor.insert("committed");
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        editor.undo();
        assert_eq!(
            editor.get_text(),
            "",
            "submit clears undo so committed text stays committed"
        );
    }

    /// Regression: a remote/mobile client (SSH, soft keyboard) that encodes
    /// its Enter key as LF (0x0A) or CR (0x0D) sends it as a bare character
    /// event in raw mode. Those used to fall through to the generic char arm
    /// and insert a literal newline — "回车变成了换行，无法发送内容". They must
    /// submit just like `KeyCode::Enter`.
    #[test]
    fn test_remote_lf_cr_enter_submits() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for code in [
            crossterm::event::KeyCode::Char('\n'),
            crossterm::event::KeyCode::Char('\r'),
        ] {
            let editor = Editor::simple();
            let submits = Arc::new(AtomicUsize::new(0));
            let submits_for_cb = submits.clone();
            editor.on_submit(Arc::new(move |_text: &str| {
                submits_for_cb.fetch_add(1, Ordering::SeqCst);
            }));
            editor.insert("hello");
            editor.handle_key(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert_eq!(submits.load(Ordering::SeqCst), 1, "{code:?} must submit");
            assert_eq!(editor.get_text(), "", "editor cleared after submit");
        }
    }
    #[test]
    fn test_editor_text() {
        let editor = Editor::simple();
        editor.set_text("Hello");
        assert_eq!(editor.get_text(), "Hello");
    }

    #[test]
    fn test_editor_insert() {
        let editor = Editor::simple();
        editor.insert("Hello");
        assert_eq!(editor.get_text(), "Hello");
        editor.insert(" World");
        assert_eq!(editor.get_text(), "Hello World");
    }

    #[test]
    fn test_editor_clear() {
        let editor = Editor::simple();
        editor.set_text("Test");
        editor.clear();
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn test_editor_render_has_borders() {
        // The editor renders a native-pi bordered box: a full-width `─` line
        // above and below the content, with no `> ` prompt prefix on the text
        // lines (mirrors TS editor.ts:525-588).
        let editor = Editor::new(
            EditorOptions {
                padding_x: 1,
                ..Default::default()
            },
            EditorStyle::default(),
            Arc::new(Keybindings::new()),
        );
        editor.set_text("hi");
        let lines = editor.render(20);
        assert_eq!(lines.len(), 3, "expected [border, content, border]");
        // Both borders are a 20-wide `─` run (visible width, ANSI stripped).
        let top = crate::ansi::strip_ansi(&lines[0]);
        let bottom = crate::ansi::strip_ansi(lines.last().unwrap());
        assert_eq!(
            top.chars().filter(|c| *c == '─').count(),
            20,
            "top border not full width: {top:?}"
        );
        assert_eq!(
            bottom.chars().filter(|c| *c == '─').count(),
            20,
            "bottom border not full width: {bottom:?}"
        );
        // The content line carries the text with no `> ` prompt.
        let content = crate::ansi::strip_ansi(&lines[1]);
        assert!(
            content.contains("hi"),
            "content line missing text: {content:?}"
        );
        assert!(
            !content.contains("> "),
            "content line should not have a prompt prefix: {content:?}"
        );
    }

    #[test]
    fn test_editor_multibyte_no_panic() {
        // Inserting a multibyte char (读 = 3 bytes) and then a second char
        // used to panic: `cursor_col += 1` left the caret at byte 1, inside
        // '读', so the next `String::insert(1, …)` hit a char boundary. This
        // exercises the byte-cursor fix across insert + render + a second
        // keystroke.
        let editor = Editor::simple();
        editor.set_focused(true);
        editor.insert("读");
        editor.insert("a");
        editor.insert("书");
        assert_eq!(editor.get_text(), "读a书");
        // cursor_col should be at the end (byte offset 7: 读=3 + a=1 + 书=3).
        assert_eq!(editor.cursor_position(), (0, 7));
        // Render must not panic when placing the cursor marker on the row.
        let lines = editor.render(20);
        assert_eq!(
            lines.len(),
            3,
            "bordered box keeps [border, content, border]"
        );

        // Backspace / left/right must also stay on boundaries.
        editor.backspace(); // remove 书
        assert_eq!(editor.get_text(), "读a");
        editor.cursor_left(); // left of 'a'
        assert_eq!(editor.cursor_position(), (0, 3));
        editor.insert("b"); // insert between 读 and a
        assert_eq!(editor.get_text(), "读ba");
    }

    #[test]
    fn test_editor_wraps_long_draft_inside_width() {
        let editor = Editor::new(
            EditorOptions {
                padding_x: 1,
                ..Default::default()
            },
            EditorStyle::default(),
            Arc::new(Keybindings::new()),
        );
        editor.set_focused(true);
        editor.set_text("12345678901234567890");
        let lines = editor.render(10);
        assert_eq!(lines.len(), 5, "two wrapped content rows plus borders");
        for line in &lines {
            assert!(
                crate::ansi::visible_width(line) <= 10,
                "line escaped editor width: {line:?}"
            );
        }
        let rendered = crate::ansi::strip_ansi(&lines.join("\n"));
        assert!(rendered.contains("1234567"));
        assert!(rendered.contains("90123456"));
    }

    /// The wrap point must not depend on where the caret is.
    ///
    /// Reserving the cursor cell inside the *wrapping* width for the caret's
    /// row only made the same text reflow one column earlier while it was
    /// being edited than after the caret moved away — the row grew a line, the
    /// dock changed height and the transcript jumped. Upstream avoids this by
    /// computing one `layoutWidth` for every row (`editor.ts:489`).
    #[test]
    fn wrap_and_height_are_independent_of_the_cursor_row() {
        for padding_x in [0usize, 1, 2] {
            for width in [10usize, 13, 40] {
                let editor = Editor::new(
                    EditorOptions {
                        padding_x,
                        ..Default::default()
                    },
                    EditorStyle::default(),
                    Arc::new(Keybindings::new()),
                );
                editor.set_focused(true);
                let content_width = width.saturating_sub(padding_x).max(1);
                // `content_width - 1` is the widest wrap that still leaves the
                // caret a cell; the two lengths around it straddle the boundary
                // where a caret-sensitive width would flip the height.
                for len in [content_width - 2, content_width - 1, content_width] {
                    let text = "x".repeat(len);
                    let on_caret_row = {
                        editor.set_text(&text);
                        editor.set_cursor(0, len);
                        editor.render(width).len()
                    };
                    let off_caret_row = {
                        // Same logical line, caret parked on a second line so
                        // row 0 renders without it.
                        editor.set_text(&format!("{text}\nz"));
                        editor.set_cursor(1, 1);
                        editor.render(width).len() - 1
                    };
                    assert_eq!(
                        on_caret_row, off_caret_row,
                        "padding_x={padding_x} width={width} len={len}: the caret row wrapped "
                    );
                    let rows = {
                        editor.set_text(&text);
                        editor.set_cursor(0, len);
                        editor.render(width)
                    };
                    for row in &rows {
                        assert!(
                            crate::ansi::visible_width(row) <= width,
                            "padding_x={padding_x} width={width} len={len}: row escaped the box: {row:?}"
                        );
                    }
                }
            }
        }
    }

    /// Every interior row must reach the same right edge, caret row included.
    ///
    /// The caret's cell was reserved *twice* for the row holding it — once by
    /// the wrap width and once by trimming its right padding — which left that
    /// row one column short of the box. The notch moved with the caret, so the
    /// border's right edge visibly stepped in and out while typing.
    #[test]
    fn the_caret_row_reaches_the_same_right_edge_as_the_other_rows() {
        for padding_x in [0usize, 1, 3] {
            let editor = Editor::new(
                EditorOptions {
                    padding_x,
                    ..Default::default()
                },
                EditorStyle::default(),
                Arc::new(Keybindings::new()),
            );
            editor.set_focused(true);
            let width = 20;
            editor.set_text("hi\nthere");
            // Caret on row 1: row 0 is then an ordinary row in the same frame,
            // so both edges are measured against the same render call.
            editor.set_cursor(1, 5);
            let lines = editor.render(width);
            let interior: Vec<usize> = lines[1..lines.len() - 1]
                .iter()
                .map(|line| crate::ansi::visible_width(line))
                .collect();
            assert_eq!(
                interior,
                vec![width, width],
                "padding_x={padding_x}: rows of one frame must share a right edge"
            );

            // And the caret row's marker stays strictly inside the box.
            let position = crate::extract_cursor_position(&lines, lines.len())
                .expect("the caret row carries a marker");
            assert!(
                position.1 < width,
                "padding_x={padding_x}: caret column {} must stay inside a {width}-wide box",
                position.1
            );
        }
    }

    #[test]
    fn test_editor_preserves_trailing_newline() {
        let editor = Editor::simple();
        editor.set_text("first\n");
        assert_eq!(editor.get_text(), "first\n");
        assert_eq!(editor.cursor_position(), (0, 0));
    }

    #[test]
    fn test_autocomplete_cursor_origin() {
        // Origin of the autocomplete panic (autocomplete.rs:279, before the
        // fix): the editor handed back a byte `cursor_col` for multibyte
        // text, and the slash-command provider sliced `&input[..cursor]`
        // mid-character. A multibyte editor line must still round-trip
        // through `get_suggestions` without panicking.
        use crate::autocomplete::AutocompleteManager;

        let editor = Editor::simple();
        editor.set_text("读");
        editor.set_cursor(0, 3); // byte end of line
        let text = editor.get_text();
        let (_r, col) = editor.cursor_position();
        let cursor = col.min(text.len());
        let mgr = AutocompleteManager::new();

        // No slash prefix → None; must not panic on the multibyte slice.
        assert!(mgr.get_suggestions(&text, cursor).is_none());
        // And the slash path with a char-count overshoot also must not panic.
        editor.set_text("/读");
        editor.set_cursor(0, 4);
        let text2 = editor.get_text();
        let mgr2 = AutocompleteManager::new();
        let _ = mgr2.get_suggestions(&text2, 9); // overshoot past end
    }

    #[test]
    fn insert_normalizes_crlf_and_lone_cr_to_newlines() {
        // Pasted Windows text (and the Unix `Event::Paste` payload, which keeps
        // CRLF) used to insert literal `\r` bytes into the line buffer: they
        // render as garbage and desync `cursor_col` from the string length.
        let editor = Editor::simple();
        editor.insert("line1\r\nline2\rline3");
        assert_eq!(editor.get_text(), "line1\nline2\nline3");

        // The buffer is 3 lines and the cursor sits at the end of the last.
        let (row, col) = editor.cursor_position();
        assert_eq!((row, col), (2, "line3".len()));

        // Editing after a normalized paste stays sound (no mid-char split).
        editor.insert("!");
        assert_eq!(editor.get_text(), "line1\nline2\nline3!");
    }

    #[test]
    fn insert_of_a_lone_crlf_makes_exactly_one_newline() {
        let editor = Editor::simple();
        editor.insert("a\r\nb");
        assert_eq!(editor.get_text().matches('\n').count(), 1);
        assert_eq!(editor.get_text(), "a\nb");
    }

    // ---- Folded pastes (upstream `handlePaste` / `expandPasteMarkers`) ----

    #[test]
    fn short_paste_is_inserted_verbatim() {
        let editor = Editor::simple();
        editor.handle_paste("hello\nworld\nthird");
        assert_eq!(editor.get_text(), "hello\nworld\nthird");
        assert_eq!(editor.get_expanded_text(), "hello\nworld\nthird");
    }

    #[test]
    fn folded_paste_snapshots_share_payload_across_undo_and_redo() {
        let editor = Editor::simple();
        let payload = "x".repeat(1024 * 1024);
        editor.handle_paste(&payload);
        let original_ptr = editor.pastes.lock().unwrap()[&1].content.as_ptr();
        editor.insert(&"a".repeat(64));
        for _ in 0..64 {
            editor.handle_key(crossterm::event::KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE,
            ));
        }
        assert_eq!(editor.get_expanded_text(), payload);
        {
            let undo = editor.undo_stack.lock().unwrap();
            let stored_pastes: Vec<_> = undo
                .states()
                .filter_map(|snapshot| snapshot.pastes.get(&1))
                .collect();
            assert_eq!(stored_pastes.len(), 64);
            for entry in stored_pastes {
                assert_eq!(
                    entry.content.as_ptr(),
                    original_ptr,
                    "undo snapshots must share the unchanged folded-paste body"
                );
            }
        }
        editor.undo();
        assert_eq!(editor.get_expanded_text(), format!("{payload}a"));
        assert_eq!(
            editor.pastes.lock().unwrap()[&1].content.as_ptr(),
            original_ptr
        );
        assert_eq!(
            editor.redo_stack.lock().unwrap()[0].pastes[&1]
                .content
                .as_ptr(),
            original_ptr
        );
        editor.redo();
        assert_eq!(editor.get_expanded_text(), payload);
        assert_eq!(
            editor.pastes.lock().unwrap()[&1].content.as_ptr(),
            original_ptr
        );
    }

    #[test]
    fn folded_paste_body_is_released_after_undo_and_redo_owners_drop() {
        let editor = Editor::simple();
        let payload = "x".repeat(2000);
        editor.handle_paste(&payload);
        let weak = Arc::downgrade(&editor.pastes.lock().unwrap()[&1].content);

        editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        ));
        assert!(editor.pastes.lock().unwrap().is_empty());
        assert!(
            weak.upgrade().is_some(),
            "undo must retain the deleted paste"
        );
        editor.undo();
        assert_eq!(editor.get_expanded_text(), payload);
        editor.redo();
        assert!(editor.pastes.lock().unwrap().is_empty());
        assert!(weak.upgrade().is_some());

        drop(editor);
        assert!(
            weak.upgrade().is_none(),
            "no global owner may retain the body"
        );
    }

    #[test]
    fn paste_session_replacement_keeps_snapshot_body_immutable() {
        let editor = Editor::simple();
        let payload = "x".repeat(2000);
        let mut session = editor.start_paste_session(&payload);
        let before = editor.snapshot_state();
        assert!(Arc::ptr_eq(
            &before.pastes[&1].content,
            &editor.pastes.lock().unwrap()[&1].content,
        ));

        assert!(editor.append_paste_session(&mut session, "tail"));
        assert_eq!(before.pastes[&1].content.as_ref(), payload);
        assert_eq!(editor.get_expanded_text(), format!("{payload}tail"));
        assert!(!Arc::ptr_eq(
            &before.pastes[&1].content,
            &editor.pastes.lock().unwrap()[&1].content,
        ));
        editor.undo();
        assert_eq!(editor.get_expanded_text(), "");
        editor.redo();
        assert_eq!(editor.get_expanded_text(), format!("{payload}tail"));
        assert_eq!(before.pastes[&1].content.as_ref(), payload);
    }

    #[test]
    fn many_line_paste_folds_to_a_marker() {
        let editor = Editor::simple();
        let payload = (1..=12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        editor.handle_paste(&payload);

        // The document shows only the marker; the body is stashed.
        assert_eq!(editor.get_text(), "[paste #1 +12 lines]");
        // Submitting reveals the real content.
        assert_eq!(editor.get_expanded_text(), payload);
    }

    #[test]
    fn long_single_line_paste_folds_by_char_count() {
        let editor = Editor::simple();
        let payload = "x".repeat(2000);
        editor.handle_paste(&payload);
        assert_eq!(editor.get_text(), "[paste #1 2000 chars]");
        assert_eq!(editor.get_expanded_text(), payload);
    }

    #[test]
    fn pasted_crlf_and_tabs_are_normalized_before_folding() {
        let editor = Editor::simple();
        let payload = (1..=12)
            .map(|i| format!("a\tb{i}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        editor.handle_paste(&payload);
        let expanded = editor.get_expanded_text();
        assert!(!expanded.contains('\r'), "CR must be normalized away");
        assert!(!expanded.contains('\t'), "tab must expand to spaces");
        // `a` occupies 1 column, so the tab advances to stop 8: 7 spaces.
        // (Before the tab-stop fix this was a fixed four spaces.)
        assert!(expanded.contains("a       b1"), "tab advances to column 8");
        assert_eq!(expanded.split('\n').count(), 12);
    }

    #[test]
    fn two_folded_pastes_get_distinct_ids_and_both_expand() {
        let editor = Editor::simple();
        let first = "1\n".repeat(11);
        let second = "2\n".repeat(11);
        editor.handle_paste(&first);
        editor.handle_paste(&second);
        assert_eq!(
            editor.get_text(),
            "[paste #1 +12 lines][paste #2 +12 lines]"
        );
        assert_eq!(editor.get_expanded_text(), format!("{first}{second}"));
    }

    #[test]
    fn backspace_over_a_folded_marker_drops_it_and_renumbers() {
        let editor = Editor::simple();
        let first = "a\n".repeat(11);
        let second = "b\n".repeat(11);
        editor.handle_paste(&first);
        editor.handle_paste(&second);
        assert_eq!(
            editor.get_text(),
            "[paste #1 +12 lines][paste #2 +12 lines]"
        );

        // Delete the FIRST marker: one step left lands the caret right after
        // marker #1 (markers are adjacent), and Backspace eats it whole.
        let _ = editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        ));
        let _ = editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        ));

        // Only the second survives, renumbered to #1, and it still expands.
        assert_eq!(editor.get_text(), "[paste #1 +12 lines]");
        assert_eq!(editor.get_expanded_text(), second);
    }

    #[test]
    fn undo_restores_a_backspaced_folded_paste() {
        let editor = Editor::simple();
        let payload = "line\n".repeat(11);
        editor.handle_paste(&payload);
        assert_eq!(editor.get_text(), "[paste #1 +12 lines]");

        let _ = editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        ));
        assert_eq!(editor.get_text(), "", "marker deleted");

        // Undo must resurrect both the marker AND its stashed body.
        editor.undo();
        assert_eq!(editor.get_text(), "[paste #1 +12 lines]");
        assert_eq!(editor.get_expanded_text(), payload);
    }

    #[test]
    fn cursor_arrows_step_over_a_marker_atomically() {
        let editor = Editor::simple();
        let payload = "z\n".repeat(11);
        editor.handle_paste(&payload);
        // Caret is after the marker; one step left must land BEFORE it.
        let _ = editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        ));
        assert_eq!(
            editor.cursor_position(),
            (0, 0),
            "left jumps the whole marker"
        );
        // And one step right must jump back over the whole marker.
        let _ = editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        ));
        assert_eq!(
            editor.cursor_position(),
            (0, "[paste #1 +12 lines]".len()),
            "right jumps the whole marker"
        );
    }

    #[test]
    fn clear_and_set_text_forget_folded_pastes() {
        let editor = Editor::simple();
        editor.handle_paste(&"m\n".repeat(11));
        editor.clear();
        assert_eq!(editor.get_text(), "");
        assert_eq!(editor.get_expanded_text(), "");

        editor.handle_paste(&"n\n".repeat(11));
        editor.set_text("fresh");
        // A programmatic replacement drops the store; a leftover marker would
        // otherwise expand against a stale body.
        assert_eq!(editor.get_text(), "fresh");
        assert_eq!(editor.get_expanded_text(), "fresh");
    }

    #[test]
    fn paste_session_prefix_large_body_and_tail_become_one_marker() {
        let editor = Editor::simple();
        let mut session = editor.start_paste_session("prefix:");
        assert_eq!(editor.get_text(), "prefix:", "first chunk is immediate");
        let body = "\nbody".repeat(12);
        assert!(editor.append_paste_session(&mut session, &body));
        assert_eq!(session.paste_id, Some(1));
        assert!(editor.append_paste_session(&mut session, ":tail"));
        assert_eq!(editor.get_text(), "[paste #1 +13 lines]");
        assert_eq!(editor.get_expanded_text(), format!("prefix:{body}:tail"));
        assert_eq!(editor.pastes.lock().unwrap().len(), 1);
    }

    #[test]
    fn paste_session_small_chunks_cross_exact_fold_thresholds() {
        let editor = Editor::simple();
        let mut session = editor.start_paste_session(&"界".repeat(250));
        for _ in 0..3 {
            assert!(editor.append_paste_session(&mut session, &"界".repeat(250)));
        }
        assert_eq!(editor.get_text(), "界".repeat(1000));
        assert_eq!(session.paste_id, None);
        assert!(editor.append_paste_session(&mut session, "😀"));
        assert_eq!(editor.get_text(), "[paste #1 1001 chars]");
        assert_eq!(
            editor.get_expanded_text(),
            format!("{}😀", "界".repeat(1000))
        );

        let editor = Editor::simple();
        let mut session = editor.start_paste_session("a");
        for _ in 0..9 {
            assert!(editor.append_paste_session(&mut session, "\na"));
        }
        assert_eq!(editor.get_text().split('\n').count(), 10);
        assert_eq!(session.paste_id, None);
        assert!(editor.append_paste_session(&mut session, "\na"));
        assert_eq!(editor.get_text(), "[paste #1 +11 lines]");
        assert_eq!(editor.get_expanded_text(), format!("a{}", "\na".repeat(10)));
    }

    #[test]
    fn paste_session_folded_chunks_reuse_id_and_update_marker() {
        let editor = Editor::simple();
        let first = "x".repeat(1001);
        let mut session = editor.start_paste_session(&first);
        assert_eq!(editor.get_text(), "[paste #1 1001 chars]");
        assert!(editor.append_paste_session(&mut session, "tail"));
        assert_eq!(editor.get_text(), "[paste #1 1005 chars]");
        assert!(editor.append_paste_session(&mut session, &"\ny".repeat(11)));
        assert_eq!(editor.get_text(), "[paste #1 +12 lines]");
        assert_eq!(session.paste_id, Some(1));
        assert_eq!(*editor.paste_counter.lock().unwrap(), 1);
        assert_eq!(editor.pastes.lock().unwrap().len(), 1);
        assert_eq!(
            editor.get_expanded_text(),
            format!("{first}tail{}", "\ny".repeat(11))
        );
    }

    #[test]
    fn paste_session_replaces_only_owned_span_in_multiline_draft() {
        let editor = Editor::simple();
        editor.set_text("草稿😀末尾\nkeep");
        editor.set_cursor(0, "草稿".len());
        let mut session = editor.start_paste_session("中\n间");
        assert_eq!(editor.get_text(), "草稿中\n间😀末尾\nkeep");
        assert!(editor.append_paste_session(&mut session, "\n尾"));
        assert_eq!(editor.get_text(), "草稿中\n间\n尾😀末尾\nkeep");
        assert!(editor.append_paste_session(&mut session, &"\nlarge".repeat(11)));
        assert_eq!(editor.get_text(), "草稿[paste #1 +14 lines]😀末尾\nkeep");
        assert_eq!(
            editor.get_expanded_text(),
            format!("草稿中\n间\n尾{}😀末尾\nkeep", "\nlarge".repeat(11))
        );
    }

    #[test]
    fn paste_session_selection_and_existing_paste_preserve_surroundings() {
        let editor = Editor::simple();
        let other = "other\n".repeat(11);
        editor.handle_paste(&other);
        editor.insert("\nleft删除\n选区right\nuntouched");
        editor.set_cursor(1, "left".len());
        editor.begin_selection();
        // Select across rows, leaving both outer fragments intact.
        {
            let mut state = editor.state.lock().unwrap();
            state.cursor_row = 2;
            state.cursor_col = "选区".len();
        }
        let mut session = editor.start_paste_session("新");
        assert_eq!(
            editor.get_text(),
            "[paste #1 +12 lines]\nleft新right\nuntouched"
        );
        let body = "\nchunk".repeat(11);
        assert!(editor.append_paste_session(&mut session, &body));
        assert!(editor.append_paste_session(&mut session, "尾"));
        assert_eq!(
            editor.get_text(),
            "[paste #1 +12 lines]\nleft[paste #2 +12 lines]right\nuntouched"
        );
        assert_eq!(
            editor.get_expanded_text(),
            format!("{other}\nleft新{body}尾right\nuntouched")
        );
        assert_eq!(
            editor
                .pastes
                .lock()
                .unwrap()
                .get(&1)
                .unwrap()
                .content
                .as_ref(),
            other
        );
        assert_eq!(editor.pastes.lock().unwrap().len(), 2);
        editor.undo();
        assert_eq!(
            editor.get_text(),
            "[paste #1 +12 lines]\nleft删除\n选区right\nuntouched"
        );
        assert_eq!(editor.pastes.lock().unwrap().len(), 1);
    }

    #[test]
    fn paste_session_undo_redo_is_atomic_and_separate_from_typing() {
        let editor = Editor::simple();
        editor.insert("before");
        let mut session = editor.start_paste_session("first");
        let body = "\nbody".repeat(11);
        assert!(editor.append_paste_session(&mut session, &body));
        assert!(editor.append_paste_session(&mut session, "tail"));
        editor.insert("after");
        editor.undo();
        assert_eq!(editor.get_text(), "before[paste #1 +12 lines]");
        editor.undo();
        assert_eq!(editor.get_text(), "before");
        assert!(editor.pastes.lock().unwrap().is_empty());
        editor.undo();
        assert_eq!(editor.get_text(), "");
        editor.redo();
        assert_eq!(editor.get_text(), "before");
        editor.redo();
        assert_eq!(editor.get_text(), "before[paste #1 +12 lines]");
        assert_eq!(editor.get_expanded_text(), format!("beforefirst{body}tail"));
        editor.redo();
        assert_eq!(
            editor.get_expanded_text(),
            format!("beforefirst{body}tailafter")
        );
    }

    /// Invalid continuations must not touch anything, including a foreign
    /// editor's folded store, redo history, or the token's accumulated raw text.
    fn assert_paste_session_rejected(editor: &Editor, session: &mut PasteSession) {
        let before = editor.snapshot_state();
        let expanded = editor.get_expanded_text();
        let raw = session.raw.clone();
        let revision = session.revision;
        let action = *editor.last_action.lock().unwrap();
        let selection = editor.state.lock().unwrap().selection_anchor;
        let undo_len = editor.undo_stack.lock().unwrap().len();
        let redo_len = editor.redo_stack.lock().unwrap().len();
        assert!(!editor.append_paste_session(session, &"bad\n".repeat(12)));
        let after = editor.snapshot_state();
        assert_eq!(before.lines, after.lines);
        assert_eq!(before.cursor_row, after.cursor_row);
        assert_eq!(before.cursor_col, after.cursor_col);
        assert_eq!(before.paste_counter, after.paste_counter);
        assert_eq!(before.pastes.len(), after.pastes.len());
        for (id, entry) in before.pastes {
            assert_eq!(entry.content, after.pastes.get(&id).unwrap().content);
        }
        assert_eq!(editor.get_expanded_text(), expanded);
        assert_eq!(session.raw, raw);
        assert_eq!(session.revision, revision);
        assert_eq!(*editor.last_action.lock().unwrap(), action);
        assert_eq!(editor.state.lock().unwrap().selection_anchor, selection);
        assert_eq!(editor.undo_stack.lock().unwrap().len(), undo_len);
        assert_eq!(editor.redo_stack.lock().unwrap().len(), redo_len);
    }

    #[test]
    fn paste_session_change_observer_cursor_placement_invalidates_token() {
        let editor = Arc::new(Editor::simple());
        let weak = Arc::downgrade(&editor);
        let observer = Arc::new(move |_: &str| {
            let editor = weak.upgrade().unwrap();
            let (row, col) = editor.cursor_position();
            editor.set_cursor(row, col);
        });
        editor.on_change(observer.clone());
        let mut session = editor.start_paste_session("first");
        assert_paste_session_rejected(&editor, &mut session);

        *editor.on_change.lock().unwrap() = None;
        let mut session = editor.start_paste_session("second");
        editor.on_change(observer);
        assert!(editor.append_paste_session(&mut session, "tail"));
        assert_paste_session_rejected(&editor, &mut session);
    }

    #[test]
    fn paste_session_cursor_moves_and_key_events_permanently_invalidate() {
        let editor = Editor::simple();
        let mut session = editor.start_paste_session("paste");
        let end = editor.cursor_position();
        editor.set_cursor(0, 0);
        editor.set_cursor(end.0, end.1);
        assert_paste_session_rejected(&editor, &mut session);

        let mut session = editor.start_paste_session("new");
        // Even a no-op/unhandled key invalidates the continuation.
        editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::F(12),
            KeyModifiers::NONE,
        ));
        assert_paste_session_rejected(&editor, &mut session);

        let mut session = editor.start_paste_session("new");
        editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        ));
        editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        ));
        assert_paste_session_rejected(&editor, &mut session);
    }

    #[test]
    fn paste_session_edit_restored_text_and_undo_redo_never_revive_token() {
        let editor = Editor::simple();
        let mut session = editor.start_paste_session(&"x\n".repeat(11));
        let original = editor.get_text();
        editor.insert("z");
        editor.backspace();
        assert_eq!(editor.get_text(), original);
        assert_paste_session_rejected(&editor, &mut session);

        let mut session = editor.start_paste_session("next");
        let original = editor.get_text();
        editor.undo();
        assert_paste_session_rejected(&editor, &mut session);
        editor.redo();
        assert_eq!(editor.get_text(), original);
        assert_paste_session_rejected(&editor, &mut session);
    }

    #[test]
    fn paste_session_clear_set_text_and_foreign_owner_are_rejected() {
        let editor = Editor::simple();
        let mut session = editor.start_paste_session(&"x\n".repeat(11));
        editor.clear();
        assert_paste_session_rejected(&editor, &mut session);
        let mut session = editor.start_paste_session("short");
        editor.set_text("short");
        assert_paste_session_rejected(&editor, &mut session);

        let foreign = Editor::simple();
        let mut session = editor.start_paste_session(&"a\n".repeat(11));
        foreign.handle_paste(&"b\n".repeat(11));
        assert_paste_session_rejected(&foreign, &mut session);
        // A wrong-owner attempt must not consume the valid owner's token.
        assert!(editor.append_paste_session(&mut session, "own"));

        let mut old = editor.start_paste_session("old");
        let _new = editor.start_paste_session("");
        assert_paste_session_rejected(&editor, &mut old);
    }

    #[test]
    fn paste_session_checks_selection_expected_cursor_and_owned_display() {
        // Test the defense-in-depth checks independently of revision tracking.
        let editor = Editor::simple();
        let mut session = editor.start_paste_session("你好😀");
        editor.begin_selection();
        assert_paste_session_rejected(&editor, &mut session);
        editor.clear_selection();
        editor.state.lock().unwrap().cursor_col = 0;
        assert_paste_session_rejected(&editor, &mut session);
        editor.state.lock().unwrap().cursor_col = session.end.1;
        editor.state.lock().unwrap().lines[0] = "different".to_string();
        assert_paste_session_rejected(&editor, &mut session);
    }

    #[test]
    fn expand_tabs_advances_to_the_next_tab_stop_per_line() {
        // Column 0: 7 spaces to reach the stop at 8.
        assert_eq!(expand_tabs("\t", 8), " ".repeat(8));
        // Column 2: 6 spaces to reach 8; column 10: 6 spaces to reach 16.
        assert_eq!(expand_tabs("if\t", 8), format!("if{}", " ".repeat(6)));
        assert_eq!(
            expand_tabs("0123456789\t", 8),
            format!("0123456789{}", " ".repeat(6))
        );
        // Consecutive stops.
        assert_eq!(expand_tabs("\t\tx", 8), format!("{}x", " ".repeat(16)));
        // A stop already reached is not another stop away.
        assert_eq!(
            expand_tabs("12345678\t", 8),
            format!("12345678{}", " ".repeat(8))
        );
        // The column resets at a newline: line 2's tab lands on stop 8 again.
        assert_eq!(expand_tabs("a\tb\nc\td", 8), "a       b\nc       d");
    }

    #[test]
    fn expand_tabs_counts_wide_chars_as_two_columns() {
        // "中文" is 4 columns; the tab must land on stop 8, not stop 5.
        assert_eq!(expand_tabs("中文\t", 8), format!("中文{}", " ".repeat(4)));
        // An emoji (width 2) after a tab still counts as 2 columns of progress.
        assert_eq!(
            expand_tabs("\t\u{1F600}x", 8),
            format!("{}\u{1F600}x", " ".repeat(8))
        );
    }

    #[test]
    fn expand_tabs_measures_unicode_sequences_not_scalars() {
        // `UnicodeWidthStr` is not additive across the scalars of one grapheme
        // cluster. Under the locked unicode-width 0.1.14, `❤️` (VS16
        // presentation sequence) is 2 columns as a whole but 1 when its
        // scalars are summed, and `👩‍💻` (ZWJ sequence) is 2 as a whole but 4
        // when summed. Measuring scalars therefore both under- and over-counted
        // the column, which moved the tab stop — and the text handed to the
        // model, since the expanded paste is what gets sent.
        //
        // The expected space counts below are an independent oracle: each
        // sequence occupies exactly 2 terminal columns, so a tab at column 2
        // advances 6 spaces to stop 8. They are written out literally rather
        // than derived from the width crate, so a future width regression
        // cannot make the test agree with the bug.
        for seq in ["❤️", "👩‍💻", "👍🏽", "🇨🇳", "1️⃣"] {
            assert_eq!(
                expand_tabs(&format!("{seq}\tX"), 8),
                format!("{seq}      X"),
                "{seq:?} must occupy 2 columns and reach stop 8"
            );
        }
        // A single-width grapheme keeps its own stop: column 1 -> 7 spaces.
        assert_eq!(expand_tabs("e\u{301}\tX", 8), "e\u{301}       X");
    }

    #[test]
    fn pasted_unicode_sequences_reach_the_same_column_through_handle_paste() {
        // The normalized text is what the model receives, so the assertion has
        // to go through the real paste path rather than `expand_tabs` alone.
        // Each 2-column sequence plus 6 spaces puts `X` at column 8.
        for seq in ["❤️", "👩‍💻", "👍🏽"] {
            let editor = Editor::simple();
            editor.handle_paste(&format!("{seq}\tX"));
            assert_eq!(
                editor.get_expanded_text(),
                format!("{seq}      X"),
                "{seq:?} tab stop must land on column 8 through handle_paste"
            );
        }
        // A 4-column CJK pair advances to stop 8 with 4 spaces.
        let editor = Editor::simple();
        editor.handle_paste("中文\tX");
        assert_eq!(editor.get_expanded_text(), "中文    X");
    }

    #[test]
    fn expand_tabs_measures_complete_spans_not_grapheme_sums() {
        use unicode_width::UnicodeWidthStr;
        // `UnicodeWidthStr` applies cross-grapheme script rules that no sum
        // over the parts can reproduce. Under the locked unicode-width 0.1.14
        // each Arabic Lam-Alef ligature below is one column as a whole while
        // its two graphemes each measure one column. The expected space counts
        // are a literal oracle (1 column + 7 spaces = stop 8), not derived
        // from the width crate, so a width regression cannot agree with a bug.
        for seq in [
            "\u{644}\u{627}",
            "\u{644}\u{623}",
            "\u{644}\u{622}",
            "\u{644}\u{671}",
        ] {
            assert_eq!(
                UnicodeWidthStr::width(seq),
                1,
                "{seq:?} must measure one column as a complete span"
            );
            assert_eq!(
                expand_tabs(&format!("{seq}\tX"), 8),
                format!("{seq}       X"),
                "{seq:?} is a 1-column span and must reach stop 8"
            );
        }
        // The span boundary is the tab, not the ligature: two ligatures are
        // two columns and still reach stop 8, and a ligature after a tab
        // advances from the stop by its own width.
        assert_eq!(
            expand_tabs("\u{644}\u{627}\u{644}\u{627}\tX", 8),
            "\u{644}\u{627}\u{644}\u{627}      X"
        );
        assert_eq!(
            expand_tabs("\t\u{644}\u{627}x", 8),
            format!("{}\u{644}\u{627}x", " ".repeat(8))
        );
    }

    #[test]
    fn pasted_lam_alef_reaches_the_span_column_through_handle_paste() {
        // The expanded paste is what the model receives, so the assertion has
        // to go through the real paste path, not `expand_tabs` alone.
        for seq in ["\u{644}\u{627}", "\u{644}\u{623}"] {
            let editor = Editor::simple();
            editor.handle_paste(&format!("{seq}\tX"));
            assert_eq!(
                editor.get_expanded_text(),
                format!("{seq}       X"),
                "{seq:?} must land `X` at column 8 through handle_paste"
            );
        }
        // Mixed with a previously measured cluster: a 2-column emoji and a
        // 1-column ligature in the same span are 3 columns, 5 spaces to stop 8.
        let editor = Editor::simple();
        editor.handle_paste("\u{1F600}\u{644}\u{627}\tX");
        assert_eq!(
            editor.get_expanded_text(),
            "\u{1F600}\u{644}\u{627}     X",
            "span widths must add across clusters but never inside one"
        );
        // The column resets at a newline, so a ligature on line 2 starts over.
        let editor = Editor::simple();
        editor.handle_paste("\u{644}\u{627}\tA\n\u{644}\u{627}\tB");
        assert_eq!(
            editor.get_expanded_text(),
            "\u{644}\u{627}       A\n\u{644}\u{627}       B"
        );
    }

    #[test]
    fn paste_session_unicode_tabs_and_split_crlf_match_single_paste() {
        let editor = Editor::simple();
        editor.set_text("前😀后");
        editor.set_cursor(0, "前".len());
        let mut session = editor.start_paste_session("中文\t😀\r");
        assert_eq!(editor.get_text(), "前中文    😀\n😀后");
        for chunk in ["\n后\r", "\n\t尾\r", "尾\u{1}\x1b[104;", "5u"] {
            assert!(editor.append_paste_session(&mut session, chunk));
        }
        let expected = "前中文    😀\n后\n        尾\n尾😀后";
        assert_eq!(editor.get_text(), expected);
        assert_eq!(editor.cursor_position(), (3, "尾".len()));
        assert_eq!(editor.get_expanded_text(), expected);
        editor.undo();
        assert_eq!(editor.get_text(), "前😀后");
    }

    #[test]
    fn paste_session_empty_initial_chunks_defer_undo_until_content() {
        let editor = Editor::simple();
        editor.insert("before");
        let mut session = editor.start_paste_session("\u{1}");
        assert_eq!(editor.get_text(), "before");
        assert!(editor.append_paste_session(&mut session, "\u{2}"));
        assert!(editor.append_paste_session(&mut session, "text"));
        assert_eq!(editor.get_text(), "beforetext");
        editor.undo();
        assert_eq!(editor.get_text(), "before");
        editor.undo();
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn csi_u_ctrl_sequences_decode_then_drop_like_native_pi() {
        // `ESC[104;5u` is kitty CSI-u for Ctrl+H. Upstream decodes it to the
        // control char (104 - 96 = 8) and then its `charCode >= 32` filter
        // drops it, so a stream of them folds to nothing.
        let editor = Editor::simple();
        editor.handle_paste(&"\x1b[104;5u".repeat(1200));
        assert_eq!(editor.get_text(), "");
        assert_eq!(editor.get_expanded_text(), "");
    }
}
