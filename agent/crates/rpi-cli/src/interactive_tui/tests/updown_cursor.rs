//! Probes for ↑/↓ inside a multi-line draft.
//!
//! Desired behaviour: inside a multi-line prompt, ↑/↓ move the caret between
//! logical lines; only when the caret already sits on the first (↑) or last
//! (↓) line does another press switch to prompt history.
//!
//! Both halves are covered: a draft the user typed, and a multi-line prompt
//! recalled from history (the recall used to park the caret at `(0, 0)`, so
//! the recalled entry could only be walked upward).

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The key loop's routing predicate (`interactive_tui.rs`, step 5b) — the unit
/// under test. Probes call it directly so they fail on the routing decision
/// itself rather than on a copy of it.
use crate::interactive_tui::state::arrow_belongs_to_history;

fn arrow(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A typed multi-line draft keeps both arrows for the caret; only the presses
/// that cannot move the caret reach history.
#[test]
fn typed_multi_line_draft_keeps_arrows_until_the_caret_hits_an_edge() {
    let state = test_tui_state();
    let editor = state.editor.clone();
    push_history(&state, "an older prompt");
    editor.set_text("first line\nsecond line");
    editor.set_cursor(1, 3);

    // On the last line, ↓ is the history step…
    assert!(arrow_belongs_to_history(&state, &editor, 1));
    // …and ↑ still belongs to the caret.
    assert!(!arrow_belongs_to_history(&state, &editor, -1));
    editor.handle_key(arrow(KeyCode::Up));
    assert_eq!(editor.cursor_position(), (0, 3), "↑ must reach line 1");

    // On the first line the roles swap.
    assert!(arrow_belongs_to_history(&state, &editor, -1));
    assert!(!arrow_belongs_to_history(&state, &editor, 1));
    editor.handle_key(arrow(KeyCode::Down));
    assert_eq!(editor.cursor_position(), (1, 3), "↓ must reach line 2");

    assert_eq!(
        editor.get_text(),
        "first line\nsecond line",
        "arrow navigation must never replace the draft"
    );
    assert_eq!(*state.history_index.lock().unwrap(), -1);
}

/// A recalled multi-line prompt is editable line by line: the recall lands on
/// the last line, ↑ walks up to the first, and each edge press then steps
/// through history.
#[test]
fn recalled_multi_line_prompt_keeps_its_lines_reachable_with_arrows() {
    let state = test_tui_state();
    let editor = state.editor.clone();
    push_history(&state, "line one\nline two");

    // Empty prompt: ↑ recalls the most recent entry.
    assert!(arrow_belongs_to_history(&state, &editor, -1));
    navigate_history(&state, &editor, -1);
    assert_eq!(*state.history_index.lock().unwrap(), 0);
    assert_eq!(
        editor.cursor_position(),
        (1, "line two".len()),
        "a recalled prompt must be edited from its tail, not from line 1"
    );

    // On the last line ↑ moves the caret, it does not leave the entry.
    assert!(!arrow_belongs_to_history(&state, &editor, -1));
    editor.handle_key(arrow(KeyCode::Up));
    assert_eq!(editor.cursor_position(), (0, "line one".len()));
    assert_eq!(
        editor.get_text(),
        "line one\nline two",
        "↑ inside the recalled entry must not swap it for another history item"
    );

    // On the first line ↑ is a history step again — and there is no older
    // entry, so the index stays put.
    assert!(arrow_belongs_to_history(&state, &editor, -1));
    navigate_history(&state, &editor, -1);
    assert_eq!(*state.history_index.lock().unwrap(), 0);
    assert_eq!(editor.get_text(), "line one\nline two");
}

/// A single-line draft keeps the pre-existing contract: the arrows never
/// replace it, because swapping a typed draft for a history entry was reported
/// as "text disappeared". Only a multi-line draft reinterprets the edge press.
#[test]
fn a_single_line_draft_never_hands_the_arrows_to_history() {
    let state = test_tui_state();
    let editor = state.editor.clone();
    push_history(&state, "recalled");

    editor.set_text("my draft");
    editor.set_cursor(0, 0);
    assert!(!arrow_belongs_to_history(&state, &editor, -1));
    assert!(!arrow_belongs_to_history(&state, &editor, 1));
    assert_eq!(*state.history_index.lock().unwrap(), -1);
    assert_eq!(editor.get_text(), "my draft");

    // Leaving browse mode is still reachable: an empty prompt recalls, and ↓
    // walks back out to the (empty) draft.
    editor.set_text("");
    assert!(arrow_belongs_to_history(&state, &editor, -1));
    navigate_history(&state, &editor, -1);
    assert_eq!(editor.get_text(), "recalled");
    navigate_history(&state, &editor, 1);
    assert_eq!(editor.get_text(), "");
    assert_eq!(*state.history_index.lock().unwrap(), -1);
}

/// With no history to browse the arrows stay with the editor even on the
/// boundary lines, so a multi-line draft never goes dead.
#[test]
fn arrows_stay_with_the_editor_when_there_is_no_history() {
    let state = test_tui_state();
    let editor = state.editor.clone();
    editor.set_text("only line");
    editor.set_cursor(0, 0);

    assert!(!arrow_belongs_to_history(&state, &editor, -1));
    assert!(!arrow_belongs_to_history(&state, &editor, 1));
    assert_eq!(*state.history_index.lock().unwrap(), -1);
}

/// Submitting a recalled entry must end the browse session even when the entry
/// is a duplicate of the newest history item — `push_history` skips the insert
/// then, and the old code skipped the browse reset with it. A stale
/// `history_index` left the next ↓ restoring a draft the user had already
/// submitted.
#[test]
fn submitting_a_recalled_entry_always_ends_the_browse_session() {
    let state = test_tui_state();
    let editor = state.editor.clone();
    push_history(&state, "the only prompt");

    navigate_history(&state, &editor, -1);
    assert_eq!(*state.history_index.lock().unwrap(), 0);

    // Resubmitting the recalled text is a duplicate → no new history entry.
    push_history(&state, "the only prompt");
    assert_eq!(state.history.lock().unwrap().len(), 1);
    assert_eq!(
        *state.history_index.lock().unwrap(),
        -1,
        "a submit must end the browse session even when history did not grow"
    );
    assert!(state.history_draft.lock().unwrap().is_none());
}

/// A recalled multi-line prompt submits as a whole: the line breaks the user
/// walked over with ↑/↓ are real draft content, not view state.
#[test]
fn arrows_over_a_recalled_prompt_keep_its_line_breaks_on_submit() {
    let state = test_tui_state();
    let editor = state.editor.clone();
    let submitted = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let sink = submitted.clone();
        editor.on_submit(Arc::new(move |text: &str| {
            sink.lock().unwrap().push(text.to_string());
        }));
    }
    push_history(&state, "alpha\nbeta");

    navigate_history(&state, &editor, -1);
    // Walk to the first line and back down, then submit.
    editor.handle_key(arrow(KeyCode::Up));
    editor.handle_key(arrow(KeyCode::Down));
    editor.handle_key(arrow(KeyCode::Enter));

    assert_eq!(
        submitted.lock().unwrap().as_slice(),
        ["alpha\nbeta"],
        "the submitted prompt must still carry both lines"
    );
}

/// Continuing a browse session with the keyboard after recalling a SINGLE-LINE
/// entry: ↓ must walk to the older entry and ↑ must come back — the routing
/// predicate, not just `navigate_history`, has to honour the browse state
/// (`history_index != -1`).
#[test]
fn single_line_recall_continues_browsing_with_both_arrows() {
    let state = test_tui_state();
    let editor = state.editor.clone();
    push_history(&state, "older prompt");
    push_history(&state, "newest prompt");

    // Fresh prompt: ↑ recalls the newest entry.
    assert!(arrow_belongs_to_history(&state, &editor, -1));
    navigate_history(&state, &editor, -1);
    assert_eq!(editor.get_text(), "newest prompt");
    assert_eq!(*state.history_index.lock().unwrap(), 0);

    // Mid-session on a single-line entry: both arrows stay with history.
    assert!(
        arrow_belongs_to_history(&state, &editor, -1),
        "↑ during an active browse session must step history, not the caret"
    );
    navigate_history(&state, &editor, -1);
    assert_eq!(editor.get_text(), "older prompt");

    assert!(
        arrow_belongs_to_history(&state, &editor, 1),
        "↓ during an active browse session must stay in history"
    );
    navigate_history(&state, &editor, 1);
    assert_eq!(editor.get_text(), "newest prompt");

    // ↓ past the newest entry ends the session and restores the draft.
    assert!(arrow_belongs_to_history(&state, &editor, 1));
    navigate_history(&state, &editor, 1);
    assert_eq!(editor.get_text(), "");
    assert_eq!(*state.history_index.lock().unwrap(), -1);
}

/// A TYPED single-line draft must still never swap itself for a history entry,
/// even though the same "one line, non-empty" shape now has to route to
/// history once a browse session is active — the two cases differ only in
/// `history_index`.
#[test]
fn typed_single_line_draft_is_still_never_replaced() {
    let state = test_tui_state();
    let editor = state.editor.clone();
    push_history(&state, "some prompt");
    editor.set_text("my draft");
    editor.set_cursor(0, 4);

    assert!(!arrow_belongs_to_history(&state, &editor, -1));
    assert!(!arrow_belongs_to_history(&state, &editor, 1));
    assert_eq!(*state.history_index.lock().unwrap(), -1);
    assert_eq!(editor.get_text(), "my draft");
}
