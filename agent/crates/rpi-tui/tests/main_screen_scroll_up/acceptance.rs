//! Acceptance contracts for the macOS/iTerm2 native-scrollback report.
//!
//! These are protocol/model tests, not an iTerm2 visual test. The timer fixture
//! uses a real tool panel and bounded waits for an observable elapsed-label change.
//! Run `cargo test -p rpi-tui --test main_screen_scroll_up acceptance::`.
//! Keep failing contracts visible: do not ignore them or assert the broken output.

use super::*;
use rpi_tui::{ansi::CURSOR_MARKER, BashExecutionComponent, Component, ToolExecutionComponent};
use std::time::{Duration, Instant};

const WIDTH: usize = 80;
const HEIGHT: usize = 12;
const HISTORY_LINES: usize = 80;

fn old_history() -> Arc<Text> {
    Arc::new(Text::new(
        &(0..HISTORY_LINES)
            .map(|i| format!("old-message-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
        0,
        0,
    ))
}

fn output(lines: usize) -> String {
    (0..lines)
        .map(|i| format!("tool-output-{i:03}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn add_dock(root: &Container) {
    let editor = Arc::new(rpi_tui::Editor::simple());
    editor.set_text("cargo test -p rpi-tui --locked");
    editor.set_focused(true);
    root.add_child(editor);
    root.add_child(Arc::new(Text::new("FOOTER-STATUS", 0, 0)));
}

fn document(term: &VirtualTerminal) -> Vec<String> {
    term.state
        .lock()
        .unwrap()
        .document
        .iter()
        .map(|row| row.iter().collect::<String>().trim_end().to_string())
        .collect()
}

fn assert_history_once(term: &VirtualTerminal) {
    let rows = document(term);
    for i in 0..HISTORY_LINES {
        let message = format!("old-message-{i:03}");
        assert_eq!(rows[i], message, "history row {i} moved or was overwritten");
        assert_eq!(
            rows.iter().filter(|row| *row == &message).count(),
            1,
            "history row {i} was duplicated"
        );
    }
}

#[test]
fn timer_only_updates_preserve_native_history_without_erasing_scrollback() {
    // e59a6bb added changed_in_scrollback -> ED(3) for this case. A changing
    // elapsed label is not new transcript content and must not destroy the
    // native history that the user is currently reading.
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    let tool = Arc::new(ToolExecutionComponent::new(
        "read",
        r#"{"path":"large-file.rs"}"#,
    ));
    tool.set_result(&output(40), false);
    tool.set_expanded(true);
    tool.set_running();
    root.add_child(tool.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root));
    tui.start_readerless();

    assert!(term.state.lock().unwrap().viewport_top > HISTORY_LINES);
    term.browse_history_at(10);
    let visible_before = term.visible();
    let erases_before = term.state.lock().unwrap().scrollback_erase_count;
    let mut erase_deltas = Vec::new();
    let mut previous_tool = tool.render(WIDTH);
    for _ in 0..3 {
        // Do not assume a fixed sleep advances the formatted timer: wait for
        // the real component to change, with a deadline so a frozen timer fails.
        // No tool output, arguments, status or draft changes during this loop.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let rendered = tool.render(WIDTH);
            if rendered != previous_tool {
                previous_tool = rendered;
                break;
            }
            assert!(Instant::now() < deadline, "live tool timer did not advance");
            std::thread::sleep(Duration::from_millis(5));
        }
        tui.request_render(false);
        erase_deltas.push(term.state.lock().unwrap().scrollback_erase_count - erases_before);
    }

    assert_eq!(
        erase_deltas,
        vec![0, 0, 0],
        "timer-only refreshes must not erase native scrollback (ED 3)"
    );
    assert_eq!(term.state.lock().unwrap().history_view_top, Some(10));
    assert_eq!(term.visible(), visible_before, "native history view jumped");
    assert_history_once(&term);
}

#[test]
fn streamed_output_growth_preserves_history_commands_and_draft() {
    // Positive control: native browsing must not prevent real output growth,
    // duplicate an already-written message, or truncate either command line.
    let command = "printf 'streamed-command-marker'";
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    let bash = Arc::new(BashExecutionComponent::new(command));
    bash.set_expanded(true);
    bash.append_output(&output(40));
    root.add_child(bash.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root));
    tui.start_readerless();

    term.browse_history_at(10);
    let visible_before = term.visible();
    let erases_before = term.state.lock().unwrap().scrollback_erase_count;
    for lines in 41..=43 {
        bash.append_output(&output(lines));
        tui.request_render(false);

        // Check every painted frame, not just a final frame that could have
        // recovered from a transient duplicate/overwrite during streaming.
        assert_eq!(
            term.state.lock().unwrap().scrollback_erase_count,
            erases_before
        );
        assert_eq!(term.state.lock().unwrap().history_view_top, Some(10));
        assert_eq!(term.visible(), visible_before);
        assert_history_once(&term);
        let rows = document(&term);
        for i in 0..lines {
            let line = format!("tool-output-{i:03}");
            assert_eq!(
                rows.iter().filter(|row| row.contains(&line)).count(),
                1,
                "streamed output missing or duplicated: {line}"
            );
        }
        for line in [command, "cargo test -p rpi-tui --locked", "FOOTER-STATUS"] {
            assert_eq!(
                rows.iter().filter(|row| row.contains(line)).count(),
                1,
                "command/draft/footer missing, truncated or duplicated: {line}"
            );
        }
    }
}

struct CommandsWithCursorAboveTail {
    cursor_row: Mutex<usize>,
    cursor_at_start: bool,
}

impl Default for CommandsWithCursorAboveTail {
    fn default() -> Self {
        Self {
            cursor_row: Mutex::new(6),
            cursor_at_start: false,
        }
    }
}

impl Component for CommandsWithCursorAboveTail {
    fn render(&self, _width: usize) -> Vec<String> {
        (0..12)
            .map(|i| {
                let mut line = format!("command {i:02} complete");
                if i == *self.cursor_row.lock().unwrap() {
                    if self.cursor_at_start {
                        line.insert_str(0, CURSOR_MARKER);
                    } else {
                        line.push_str(CURSOR_MARKER);
                    }
                }
                line
            })
            .collect()
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[test]
fn cursor_above_live_viewport_does_not_duplicate_or_drop_command_rows() {
    // af296aa / PR #33 added the cursor-above-viewport CSI T branch. SD inserts
    // blanks in the live screen; it cannot retrieve saved document rows.
    let term = VirtualTerminal::new(40, 4);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    tui.set_layout_root(Some(Arc::new(CommandsWithCursorAboveTail::default())));
    tui.start_readerless();

    let expected: Vec<_> = (0..12)
        .map(|i| format!("command {i:02} complete"))
        .collect();
    assert_eq!(document(&term), expected);
    tui.request_render(false);
    assert_eq!(
        document(&term),
        expected,
        "unchanged repaint changed history"
    );
}

#[test]
fn offscreen_cursor_can_return_without_corrupting_the_physical_ledger() {
    let term = VirtualTerminal::new(40, 4);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let commands = Arc::new(CommandsWithCursorAboveTail::default());
    tui.set_layout_root(Some(commands.clone()));
    tui.start_readerless();
    let expected = document(&term);
    assert!(!term.state.lock().unwrap().cursor_visible);

    for row in [11, 6, 9] {
        *commands.cursor_row.lock().unwrap() = row;
        tui.request_render(false);
        assert_eq!(document(&term), expected);
        let st = term.state.lock().unwrap();
        assert_eq!(st.viewport_top, 8);
        if row >= st.viewport_top {
            assert!(st.cursor_visible);
            assert_eq!(st.cursor_row + st.viewport_top, row);
            assert_eq!(st.cursor_col, "command 00 complete".len());
        } else {
            assert!(!st.cursor_visible);
        }
    }
}

#[test]
fn column_zero_cursor_and_hidden_cursor_preference_are_respected() {
    for show_cursor in [true, false] {
        let term = VirtualTerminal::new(40, 4);
        let tui = TuiAltScreen::new(Box::new(term.clone()), show_cursor, None);
        tui.set_main_screen_mode(true);
        tui.set_layout_root(Some(Arc::new(CommandsWithCursorAboveTail {
            cursor_row: Mutex::new(11),
            cursor_at_start: true,
        })));
        tui.start_readerless();
        let st = term.state.lock().unwrap();
        assert_eq!(st.cursor_col, 0, "CSI 0 C would incorrectly move right");
        assert_eq!(st.cursor_visible, show_cursor);
    }
}

#[test]
fn forced_repaints_repair_live_rows_without_appending_or_erasing_history() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    root.add_child(Arc::new(Text::new("complete tool command", 0, 0)));
    add_dock(&root);
    tui.set_layout_root(Some(root));
    tui.start_readerless();
    term.browse_history_at(10);
    let visible_before = term.visible();
    let expected = document(&term);
    let erases_before = term.state.lock().unwrap().scrollback_erase_count;
    for _ in 0..3 {
        // Simulate an externally damaged live cell without moving the physical
        // cursor. A forced repaint must repair it, not patch against equal rows.
        {
            let mut st = term.state.lock().unwrap();
            let row = st.viewport_top;
            st.document[row][0] = '?';
        }
        tui.render_now(true);
        assert_eq!(document(&term), expected);
        assert_eq!(
            term.state.lock().unwrap().scrollback_erase_count,
            erases_before
        );
        assert_eq!(term.state.lock().unwrap().history_view_top, Some(10));
        assert_eq!(term.visible(), visible_before);
        assert_history_once(&term);
    }
}

#[test]
fn forced_repaint_clears_line_suffixes_and_unused_live_rows() {
    let term = VirtualTerminal::new(40, 4);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let text = Arc::new(Text::new("short", 0, 0));
    tui.set_layout_root(Some(text.clone()));
    tui.start_readerless();
    let expected = document(&term);
    {
        let mut st = term.state.lock().unwrap();
        st.document[0][25] = '?';
        st.document[2][3] = '?';
    }
    tui.render_now(true);
    assert_eq!(document(&term), expected);
    assert_eq!(term.state.lock().unwrap().scrollback_erase_count, 0);

    // Subsequent appends must still start at the actual tail, not the blank
    // row visited while clearing the rest of the live screen.
    text.set_text("short\nnext command complete");
    tui.request_render(false);
    let rows = document(&term);
    assert_eq!(rows[0], "short");
    assert_eq!(rows[1], "next command complete");
    assert!(rows[2..].iter().all(String::is_empty));
}
