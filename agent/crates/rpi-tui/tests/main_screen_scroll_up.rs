//! Regression test for main-screen mode: when a streamed message re-wraps
//! *above* the visible viewport, the renderer must rebuild scrollback instead
//! of treating screen-scroll commands as access to saved history.
//!
//! The test drives `TuiAltScreen` through a tiny virtual terminal that models
//! the main buffer (document + viewport) so we can assert what is actually
//! visible after a long-transcript streaming update.

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use rpi_tui::{
    AssistantBlock, AssistantMessageComponent, Container, Focusable, ScrollView, ScrollViewOptions,
    Text, TuiAltScreen, TUI,
};

/// Shared virtual-terminal state so the test can keep a handle to the same
/// screen the `TuiAltScreen` writes into.
#[derive(Default)]
struct VtState {
    columns: usize,
    rows: usize,
    /// The full main-buffer document (grows as content is written).
    document: Vec<Vec<char>>,
    italics: Vec<Vec<bool>>,
    italic: bool,
    inverses: Vec<Vec<bool>>,
    inverse: bool,
    /// Document index currently at the top of the live screen.
    viewport_top: usize,
    /// Native scrollback browsing is independent of the application's cursor.
    history_view_top: Option<usize>,
    scrollback_erase_count: usize,
    cursor_row: usize,
    cursor_col: usize,
    cursor_visible: bool,
    scrollback_limit: Option<usize>,
}

#[derive(Clone)]
struct VirtualTerminal {
    state: Arc<Mutex<VtState>>,
}

impl VirtualTerminal {
    fn new(columns: usize, rows: usize) -> Self {
        let state = VtState {
            columns,
            rows,
            document: vec![vec![' '; columns]; rows],
            italics: vec![vec![false; columns]; rows],
            italic: false,
            inverses: vec![vec![false; columns]; rows],
            inverse: false,
            viewport_top: 0,
            history_view_top: None,
            scrollback_erase_count: 0,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            scrollback_limit: None,
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn columns(&self) -> usize {
        self.state.lock().unwrap().columns
    }

    fn rows(&self) -> usize {
        self.state.lock().unwrap().rows
    }

    fn ensure_row(&self, st: &mut VtState, index: usize) {
        while st.document.len() <= index {
            st.document.push(vec![' '; st.columns]);
            st.italics.push(vec![false; st.columns]);
            st.inverses.push(vec![false; st.columns]);
        }
    }

    /// Browse saved history without moving the application's live cursor.
    fn browse_history_at(&self, top: usize) {
        let mut st = self.state.lock().unwrap();
        assert!(top < st.viewport_top, "fixture must have saved history");
        st.history_view_top = Some(top);
    }

    /// Visible screen as strings (top to bottom), including native history.
    fn visible(&self) -> Vec<String> {
        let st = self.state.lock().unwrap();
        let top = st.history_view_top.unwrap_or(st.viewport_top);
        (top..top + st.rows)
            .map(|i| {
                st.document
                    .get(i)
                    .map(|row| row.iter().collect())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// A single document (scrollback) row as a string.
    fn document_row(&self, index: usize) -> String {
        let st = self.state.lock().unwrap();
        st.document
            .get(index)
            .map(|row| row.iter().collect())
            .unwrap_or_default()
    }

    fn put_char(&self, ch: char) {
        let mut st = self.state.lock().unwrap();
        // Wrap only when the cursor is *past* the right edge (the character
        // written at the last column must not wrap until the next one).
        if st.cursor_col >= st.columns {
            st.cursor_col = 0;
            st.cursor_row += 1;
            if st.cursor_row >= st.rows {
                st.cursor_row = st.rows - 1;
                st.viewport_top += 1;
                Self::limit_scrollback(&mut st);
            }
        }
        let index = st.viewport_top + st.cursor_row;
        self.ensure_row(&mut st, index);
        let col = st.cursor_col.min(st.columns - 1);
        st.document[index][col] = ch;
        st.italics[index][col] = st.italic;
        st.inverses[index][col] = st.inverse;
        st.cursor_col += 1;
    }

    fn linefeed(&self) {
        let mut st = self.state.lock().unwrap();
        st.cursor_row += 1;
        if st.cursor_row >= st.rows {
            st.cursor_row = st.rows - 1;
            st.viewport_top += 1;
            Self::limit_scrollback(&mut st);
        }
    }

    fn limit_scrollback(st: &mut VtState) {
        if let Some(limit) = st.scrollback_limit {
            let excess = st.viewport_top.saturating_sub(limit);
            st.document.drain(..excess);
            st.italics.drain(..excess);
            st.inverses.drain(..excess);
            st.viewport_top -= excess;
            st.history_view_top = st.history_view_top.map(|top| top.saturating_sub(excess));
        }
    }

    fn carriage_return(&self) {
        self.state.lock().unwrap().cursor_col = 0;
    }

    fn scroll_down(&self, n: usize) {
        let mut st = self.state.lock().unwrap();
        // SD inserts blanks at the top of the live screen and discards its
        // bottom rows. It does not bring saved scrollback back onto the screen.
        let top = st.viewport_top;
        let rows = st.rows;
        self.ensure_row(&mut st, top + rows - 1);
        let n = n.max(1).min(rows);
        for row in (n..rows).rev() {
            st.document[top + row] = st.document[top + row - n].clone();
            st.italics[top + row] = st.italics[top + row - n].clone();
            st.inverses[top + row] = st.inverses[top + row - n].clone();
        }
        for row in 0..n {
            st.document[top + row] = vec![' '; st.columns];
            st.italics[top + row] = vec![false; st.columns];
            st.inverses[top + row] = vec![false; st.columns];
        }
    }

    fn feed(&self, data: &str) {
        let bytes = data.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b'\x1b' => {
                    i += 1;
                    if i >= bytes.len() {
                        break;
                    }
                    match bytes[i] {
                        b']' => {
                            // OSC metadata (including hyperlink close) occupies
                            // no cells and ends at BEL or ST.
                            i += 1;
                            while i < bytes.len() {
                                if bytes[i] == 7 {
                                    i += 1;
                                    break;
                                }
                                if bytes[i] == 27 && bytes.get(i + 1) == Some(&b'\\') {
                                    i += 2;
                                    break;
                                }
                                i += 1;
                            }
                        }
                        b'[' => {
                            i += 1;
                            let start = i;
                            while i < bytes.len()
                                && !(bytes[i] as char).is_ascii_alphabetic()
                                && bytes[i] != b'@'
                                && bytes[i] != b'`'
                            {
                                i += 1;
                            }
                            if i >= bytes.len() {
                                break;
                            }
                            let params = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
                            let final_byte = bytes[i];
                            i += 1;
                            let nums: Vec<usize> = params
                                .split(';')
                                .map(|p| p.parse::<usize>().unwrap_or(1))
                                .collect();
                            match final_byte {
                                b'A' => {
                                    let n = nums.first().copied().unwrap_or(1);
                                    let mut st = self.state.lock().unwrap();
                                    st.cursor_row = st.cursor_row.saturating_sub(n.max(1));
                                }
                                b'B' => {
                                    let n = nums.first().copied().unwrap_or(1);
                                    let mut st = self.state.lock().unwrap();
                                    st.cursor_row = (st.cursor_row + n.max(1)).min(st.rows - 1);
                                }
                                b'C' => {
                                    let n = nums.first().copied().unwrap_or(1);
                                    let mut st = self.state.lock().unwrap();
                                    st.cursor_col = (st.cursor_col + n).min(st.columns - 1);
                                }
                                b'H' | b'f' => {
                                    let row = nums.first().copied().unwrap_or(1).saturating_sub(1);
                                    let col = nums.get(1).copied().unwrap_or(1).saturating_sub(1);
                                    let mut st = self.state.lock().unwrap();
                                    st.cursor_row = row.min(st.rows - 1);
                                    st.cursor_col = col.min(st.columns - 1);
                                }
                                b'J' => {
                                    let mut st = self.state.lock().unwrap();
                                    if nums.first() == Some(&3) {
                                        // Erase saved lines only; the live screen
                                        // and cursor stay at their screen positions.
                                        // The old native browsing anchor is gone.
                                        st.scrollback_erase_count += 1;
                                        st.history_view_top = None;
                                        let top = st.viewport_top;
                                        st.document.drain(..top);
                                        st.italics.drain(..top);
                                        st.inverses.drain(..top);
                                        st.viewport_top = 0;
                                        continue;
                                    }
                                    // Clear screen (2J): blank the visible rows.
                                    let top = st.viewport_top;
                                    for row in top..top + st.rows {
                                        self.ensure_row(&mut st, row);
                                        st.document[row] = vec![' '; st.columns];
                                        st.italics[row] = vec![false; st.columns];
                                        st.inverses[row] = vec![false; st.columns];
                                    }
                                }
                                b'K' => {
                                    let mut st = self.state.lock().unwrap();
                                    let index = st.viewport_top + st.cursor_row;
                                    self.ensure_row(&mut st, index);
                                    let start = if nums.first() == Some(&2) {
                                        0
                                    } else {
                                        st.cursor_col
                                    };
                                    for c in start..st.columns {
                                        st.document[index][c] = ' ';
                                        st.italics[index][c] = false;
                                        st.inverses[index][c] = false;
                                    }
                                }
                                b'm' => {
                                    let mut st = self.state.lock().unwrap();
                                    if params.is_empty() {
                                        st.italic = false;
                                        st.inverse = false;
                                    }
                                    let mut index = 0;
                                    while index < nums.len() {
                                        match nums[index] {
                                            0 => {
                                                st.italic = false;
                                                st.inverse = false;
                                            }
                                            23 => st.italic = false,
                                            3 => st.italic = true,
                                            7 => st.inverse = true,
                                            27 => st.inverse = false,
                                            38 | 48 if nums.get(index + 1) == Some(&5) => {
                                                index += 2
                                            }
                                            38 | 48 if nums.get(index + 1) == Some(&2) => {
                                                index += 4
                                            }
                                            _ => {}
                                        }
                                        index += 1;
                                    }
                                }
                                b'T' => {
                                    let n = nums.first().copied().unwrap_or(1);
                                    self.scroll_down(n);
                                }
                                b'h' | b'l' if params == "?25" => {
                                    self.state.lock().unwrap().cursor_visible = final_byte == b'h';
                                }
                                _ => {
                                    // Ignore unsupported sequences (2026h/l, ...).
                                }
                            }
                        }
                        _ => {}
                    }
                }
                b'\r' => {
                    self.carriage_return();
                    i += 1;
                }
                b'\n' => {
                    self.linefeed();
                    i += 1;
                }
                _ => {
                    let len = if b & 0x80 == 0 {
                        1
                    } else if b & 0xE0 == 0xC0 {
                        2
                    } else if b & 0xF0 == 0xE0 {
                        3
                    } else {
                        4
                    };
                    let end = (i + len).min(bytes.len());
                    let s = std::str::from_utf8(&bytes[i..end]).unwrap_or(" ");
                    if let Some(ch) = s.chars().next() {
                        self.put_char(ch);
                    }
                    i = end;
                }
            }
        }
    }
}

impl rpi_tui::Terminal for VirtualTerminal {
    fn info(&self) -> rpi_tui::TerminalInfo {
        rpi_tui::TerminalInfo {
            columns: self.columns(),
            rows: self.rows(),
            ..Default::default()
        }
    }

    fn write(&self, data: &str) {
        self.feed(data);
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
    fn start(
        &self,
        _on_input: Box<dyn Fn(rpi_tui::InputEvent) + Send + Sync>,
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

impl VirtualTerminal {
    fn assert_text_italic(&self, text: &str, expected: bool) {
        let st = self.state.lock().unwrap();
        let needle: Vec<char> = text.chars().collect();
        let (row, column) = st
            .document
            .iter()
            .enumerate()
            .find_map(|(row, chars)| {
                chars
                    .windows(needle.len())
                    .position(|window| window == needle)
                    .map(|col| (row, col))
            })
            .unwrap_or_else(|| {
                panic!(
                    "missing text: {text}; screen: {:?}",
                    st.document
                        .iter()
                        .map(|row| row.iter().collect::<String>())
                        .collect::<Vec<_>>()
                )
            });
        assert!(
            st.italics[row][column..column + needle.len()]
                .iter()
                .all(|italic| *italic == expected),
            "wrong italic style for {text:?}: {:?}",
            &st.italics[row][column..column + needle.len()]
        );
    }
}

#[test]
fn alternate_screen_selection_preserves_text_during_incremental_patches() {
    let term = VirtualTerminal::new(56, 8);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    let plain = [
        "First paragraph remains readable before any selection.",
        "Second paragraph has a prefix and a long intact suffix.",
        "Styled prefix colored italic words and intact suffix.",
        "Last paragraph must never change when selecting above.",
    ];
    let content = format!(
        "{}\n{}\nStyled prefix \x1b[31mcolored\x1b[0m \x1b[3mitalic words\x1b[0m and intact suffix.\n{}",
        plain[0], plain[1], plain[3]
    );
    let scroll = Arc::new(ScrollView::new(
        Arc::new(Text::new(content, 0, 0)),
        ScrollViewOptions {
            primary: true,
            ..Default::default()
        },
    ));
    tui.set_layout_root(Some(scroll));
    tui.start_readerless();

    let initial = term.visible();
    let expected: Vec<String> = plain
        .iter()
        .map(|line| format!("{line:<56}"))
        .chain(std::iter::repeat_n(" ".repeat(56), 4))
        .collect();
    assert_eq!(
        initial, expected,
        "initial screen must contain every paragraph"
    );
    let redraws = tui.full_redraws();
    let assert_screen = |selection: Option<(usize, usize, usize)>| {
        // Check the entire screen, not just the extracted selection: the old
        // reverse_video_span slice bug erased/replaced unselected suffixes.
        assert_eq!(
            term.visible(),
            initial,
            "selection must not change any text"
        );
        let st = term.state.lock().unwrap();
        for row in 0..st.rows {
            for col in 0..st.columns {
                let expected = selection
                    .is_some_and(|(y, start, end)| row == y && (start..end).contains(&col));
                assert_eq!(
                    st.inverses[st.viewport_top + row][col],
                    expected,
                    "wrong inverse at ({col}, {row}), selection {selection:?}"
                );
            }
        }
        assert_eq!(tui.full_redraws(), redraws, "must use incremental patches");
    };
    let mouse = |kind, column, row| {
        tui.handle_selection_mouse_event(&MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    };

    assert_screen(None);
    // Exercise both plain text and a span crossing color/italic resets. Each
    // mouse event renders normally; forcing a redraw would bypass the bug.
    for row in [1, 2] {
        mouse(MouseEventKind::Down(MouseButton::Left), 8, row);
        assert_screen(None);
        mouse(MouseEventKind::Drag(MouseButton::Left), 18, row);
        assert_screen(Some((row as usize, 8, 19)));
        mouse(MouseEventKind::Drag(MouseButton::Left), 36, row);
        assert_screen(Some((row as usize, 8, 37)));
        mouse(MouseEventKind::Drag(MouseButton::Left), 4, row);
        assert_screen(Some((row as usize, 4, 9)));
        mouse(MouseEventKind::Up(MouseButton::Left), 4, row);
        assert_screen(Some((row as usize, 4, 9)));
        assert_eq!(
            tui.selection_text().as_deref(),
            Some(&plain[row as usize][4..9])
        );
        tui.clear_selection();
        tui.request_render(false);
        assert_screen(None);
    }
}

#[test]
fn middle_selection_stays_in_place_while_streaming_appends_rows() {
    let term = VirtualTerminal::new(40, 12);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    let rows = |count| {
        (0..count)
            .map(|i| format!("line {i:02} content"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text = Arc::new(Text::new(rows(30), 0, 0));
    let scroll = Arc::new(ScrollView::primary(text.clone()));
    tui.set_layout_root(Some(scroll.clone()));
    tui.start_readerless();
    let redraws = tui.full_redraws();
    let mouse = |kind, column| {
        tui.handle_selection_mouse_event(&MouseEvent {
            kind,
            column,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
    };
    mouse(MouseEventKind::Down(MouseButton::Left), 5);
    text.set_text(rows(45));
    tui.request_render(false);
    mouse(MouseEventKind::Drag(MouseButton::Left), 6);
    mouse(MouseEventKind::Up(MouseButton::Left), 6);
    assert_eq!(tui.selection_text().as_deref(), Some("20"));
    assert_eq!(term.visible()[2].trim_end(), "line 20 content");
    let st = term.state.lock().unwrap();
    for col in 0..40 {
        assert_eq!(
            st.inverses[st.viewport_top + 2][col],
            (5..7).contains(&col),
            "selection drifted to column {col}"
        );
    }
    drop(st);
    assert_eq!(tui.full_redraws(), redraws);
    assert!(!scroll.is_following_end());
    tui.scroll_to_bottom();
    assert!(
        scroll.is_following_end(),
        "End explicitly resumes following output"
    );
}

#[test]
fn thinking_markdown_keeps_its_style_and_body_stays_plain_across_updates() {
    let term = VirtualTerminal::new(100, 12);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let assistant = Arc::new(AssistantMessageComponent::default());
    assistant.update_blocks(&[
        AssistantBlock::Thinking("reason *emphasis* continue thinking".into()),
        AssistantBlock::Text("the normal answer".into()),
    ]);
    tui.set_layout_root(Some(assistant.clone()));
    tui.start_readerless();
    term.assert_text_italic("continue thinking", true);
    term.assert_text_italic("the normal answer", false);

    assistant.update_blocks(&[
        AssistantBlock::Thinking("reason *emphasis* continue thinking longer".into()),
        AssistantBlock::Text("the normal answer updated".into()),
    ]);
    tui.request_render(false);
    term.assert_text_italic("continue thinking longer", true);
    term.assert_text_italic("the normal answer updated", false);
}

#[test]
fn wrapped_markdown_retains_styles_when_only_continuation_rows_change() {
    let term = VirtualTerminal::new(14, 20);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let assistant = Arc::new(AssistantMessageComponent::default());
    assistant.update_blocks(&[
        AssistantBlock::Thinking("reason".into()),
        AssistantBlock::Text("*aaaaa bbbbb ccccc* ddddd".into()),
    ]);
    tui.set_layout_root(Some(assistant.clone()));
    tui.start_readerless();
    term.assert_text_italic("ccccc", true);
    term.assert_text_italic("ddddd", false);
    assistant.update_blocks(&[
        AssistantBlock::Thinking("reason".into()),
        AssistantBlock::Text("*aaaaa bbbbb zcccc* ddddd".into()),
    ]);
    tui.request_render(false);
    term.assert_text_italic("zcccc", true);
    term.assert_text_italic("ddddd", false);
}

#[test]
fn changing_block_kinds_repaints_thinking_and_body_styles() {
    let term = VirtualTerminal::new(100, 12);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let assistant = Arc::new(AssistantMessageComponent::default());
    assistant.update_blocks(&[
        AssistantBlock::Thinking("the first block".into()),
        AssistantBlock::Text("the second block".into()),
    ]);
    tui.set_layout_root(Some(assistant.clone()));
    tui.start_readerless();
    term.assert_text_italic("the first block", true);
    term.assert_text_italic("the second block", false);
    assistant.update_blocks(&[
        AssistantBlock::Text("the first block".into()),
        AssistantBlock::Thinking("the second block".into()),
    ]);
    tui.request_render(false);
    term.assert_text_italic("the first block", false);
    term.assert_text_italic("the second block", true);
    assistant.update_blocks(&[
        AssistantBlock::Thinking("the first block".into()),
        AssistantBlock::Text("the second block".into()),
    ]);
    tui.request_render(false);
    term.assert_text_italic("the first block", true);
    term.assert_text_italic("the second block", false);
}

#[test]
fn screen_scroll_and_cursor_movement_cannot_retrieve_saved_history() {
    let term = VirtualTerminal::new(20, 4);
    term.feed("zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive");
    let top = term.state.lock().unwrap().viewport_top;
    assert_eq!(top, 2);
    term.feed("\x1b[100A\x1b[100B");
    assert_eq!(term.state.lock().unwrap().viewport_top, top);
    term.feed("\x1b[H\x1b[1T");
    assert_eq!(term.state.lock().unwrap().viewport_top, top);
    let visible = term.visible();
    assert!(visible[0].trim().is_empty());
    assert_eq!(visible[1].trim(), "two");
    assert_eq!(term.document_row(0).trim(), "zero");
}

#[test]
fn repeated_history_edits_keep_the_beginning_in_bounded_scrollback() {
    let term = VirtualTerminal::new(40, 8);
    term.state.lock().unwrap().scrollback_limit = Some(128);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let text = Arc::new(Text::new("", 0, 0));
    let transcript = |revision: usize| {
        (0..100)
            .map(|line| {
                if line == 50 {
                    format!("message 050 revision {revision}")
                } else {
                    format!("message {line:03}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    text.set_text(transcript(0));
    tui.set_layout_root(Some(text.clone()));
    tui.start_readerless();

    for revision in 1..=10 {
        text.set_text(transcript(revision));
        tui.request_render(false);
        assert_eq!(
            term.document_row(0).trim(),
            "message 000",
            "revision {revision}: repeated repaint must not evict the beginning"
        );
        assert_eq!(
            term.document_row(50).trim(),
            format!("message 050 revision {revision}")
        );
        assert_eq!(term.visible().last().unwrap().trim(), "message 099");
    }
}

#[test]
fn long_transcript_stream_rewrite_keeps_dock_visible() {
    let term = VirtualTerminal::new(40, 8);
    let tui = Arc::new(TuiAltScreen::new(Box::new(term.clone()), true, None));
    tui.set_main_screen_mode(true);

    // Build a long transcript + a real editor (which carries the cursor marker
    // that drives the final cursor positioning) + a footer marker, so the first
    // frame scrolls the viewport well past the top.
    let transcript = Arc::new(Container::new());
    for i in 0..100 {
        transcript.add_child(Arc::new(Text::new(&format!("line {i:03}"), 0, 0)));
    }
    let editor = Arc::new(rpi_tui::Editor::simple());
    editor.set_focused(true);
    let footer = Arc::new(Text::new("FOOTER-STATUS", 0, 0));
    let root = Arc::new(Container::new());
    root.add_child(transcript.clone());
    root.add_child(editor.clone());
    root.add_child(footer.clone());

    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();

    // The first frame pins the footer at the bottom of the viewport.
    let visible = term.visible();
    assert!(
        visible.last().unwrap().contains("FOOTER-STATUS"),
        "first frame should show the dock at the bottom: {visible:?}"
    );

    // Simulate a long streamed message re-wrapping *above* the visible tail:
    // replace a transcript line far above the viewport so the changed range
    // starts before the current viewport top, and also insert an extra line so
    // the write spans past the bottom of the screen (forcing a scroll).
    transcript.clear();
    for i in 0..101 {
        let text = if i == 50 {
            "line 050 REWRAPPED".to_string()
        } else if i > 50 {
            format!("line {:03}", i - 1)
        } else {
            format!("line {i:03}")
        };
        transcript.add_child(Arc::new(Text::new(&text, 0, 0)));
    }
    tui.request_render(false);

    let visible = term.visible();
    assert!(
        visible.last().unwrap().contains("FOOTER-STATUS"),
        "after a streamed rewrite above the viewport the dock must stay visible: {visible:?}"
    );
    assert!(
        visible.iter().any(|row| row.contains('─')),
        "the editor border (input area) must remain visible after the rewrite: {visible:?}"
    );
    assert!(
        term.document_row(50).contains("REWRAPPED"),
        "the rewrite must land on document row 50, not be painted at the wrong row: {:?}",
        term.document_row(50)
    );

    // A second streamed update must also keep the dock pinned; a corrupted
    // viewport bookkeeping drifts further on each frame and eventually hides it.
    transcript.clear();
    for i in 0..102 {
        let text = if i == 51 {
            "line 051 REWRAPPED".to_string()
        } else if i > 51 {
            format!("line {:03}", i - 2)
        } else {
            format!("line {i:03}")
        };
        transcript.add_child(Arc::new(Text::new(&text, 0, 0)));
    }
    tui.request_render(false);

    let visible = term.visible();
    assert!(
        visible.last().unwrap().contains("FOOTER-STATUS"),
        "the dock must remain pinned across repeated streamed rewrites: {visible:?}"
    );
}

#[path = "main_screen_scroll_up/acceptance.rs"]
mod acceptance;
