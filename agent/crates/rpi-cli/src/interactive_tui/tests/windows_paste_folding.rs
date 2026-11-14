//! Display-level regressions for console pastes split across queue batches.
use super::*;
use std::time::{Duration, Instant};

#[path = "windows_paste_wait.rs"]
mod idle_wait;
#[path = "windows_paste_folding_trace.rs"]
mod recorded_batches;

fn batch(
    run: &mut WindowsPasteRun,
    editor: &Editor,
    text: &str,
    clipboard: Option<&str>,
    at: Instant,
) {
    for ch in text.chars() {
        run.feed(ch, at);
    }
    // Model a real batch with characters queued behind its first character.
    if text.chars().count() > 1 {
        run.mark_collected();
    }
    run.flush_with_clipboard(editor, at, || clipboard.map(str::to_string));
    assert!(!run.is_buffering(), "flushing never adds an input delay");
}

#[test]
fn windows_split_paste_prefix_body_and_single_character_tail_fold_together() {
    let now = Instant::now();
    let prefix = "PREFIX:";
    let body = (0..30)
        .map(|i| format!("\nline {i}: 中文🙂"))
        .collect::<String>();
    let clipboard = format!("{prefix}{body}!");
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();

    batch(&mut run, &editor, prefix, Some(&clipboard), now);
    assert_eq!(
        editor.get_text(),
        prefix,
        "short prefixes display immediately"
    );
    batch(
        &mut run,
        &editor,
        &body,
        Some(&clipboard),
        now + Duration::from_millis(5),
    );
    assert_eq!(editor.get_text(), "[paste #1 +31 lines]");
    batch(
        &mut run,
        &editor,
        "!",
        Some(&clipboard),
        now + Duration::from_millis(10),
    );
    assert_eq!(editor.get_text(), "[paste #1 +31 lines]");
    assert_eq!(editor.get_expanded_text(), clipboard);
    editor.undo();
    assert_eq!(editor.get_text(), "");
    editor.redo();
    assert_eq!(editor.get_expanded_text(), clipboard);
}

#[test]
fn windows_split_paste_small_line_batches_cross_the_fold_threshold() {
    let now = Instant::now();
    let chunks: Vec<String> = std::iter::once("intro".to_string())
        .chain((0..12).map(|i| format!("\nrow {i}")))
        .collect();
    let clipboard = chunks.concat();
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    for (i, chunk) in chunks.iter().enumerate() {
        batch(
            &mut run,
            &editor,
            chunk,
            Some(&clipboard),
            now + Duration::from_millis(i as u64 * 5),
        );
        if i < 10 {
            assert!(!editor.get_text().contains("[paste #"));
        }
    }
    assert_eq!(editor.get_text(), "[paste #1 +13 lines]");
    assert_eq!(editor.get_expanded_text(), clipboard);
}

#[test]
fn windows_split_paste_single_line_crosses_the_character_threshold() {
    let now = Instant::now();
    let clipboard = format!("head:{}END", "x".repeat(1100));
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    batch(&mut run, &editor, "head:", Some(&clipboard), now);
    batch(
        &mut run,
        &editor,
        &"x".repeat(1100),
        Some(&clipboard),
        now + Duration::from_millis(5),
    );
    batch(
        &mut run,
        &editor,
        "END",
        Some(&clipboard),
        now + Duration::from_millis(10),
    );
    assert_eq!(editor.get_text(), "[paste #1 1108 chars]");
    assert_eq!(editor.get_expanded_text(), clipboard);
}

#[test]
fn windows_split_paste_complete_identical_pastes_stay_separate() {
    let now = Instant::now();
    let body = "\nbody".repeat(12);
    let clipboard = format!("prefix{body}");
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    for i in 0..2 {
        let at = now + Duration::from_millis(i * 10);
        batch(&mut run, &editor, "prefix", Some(&clipboard), at);
        batch(
            &mut run,
            &editor,
            &body,
            Some(&clipboard),
            at + Duration::from_millis(5),
        );
    }
    assert_eq!(
        editor.get_text(),
        "[paste #1 +13 lines][paste #2 +13 lines]"
    );
    assert_eq!(editor.get_expanded_text(), clipboard.repeat(2));
}

#[test]
fn windows_split_paste_clipboard_change_unavailability_and_mismatch_preserve_text() {
    let now = Instant::now();
    let body = "\nbody".repeat(12);
    let clipboard = format!("prefix{body}");
    for next_clipboard in [None, Some("different clipboard"), Some(clipboard.as_str())] {
        let editor = Editor::simple();
        let mut run = WindowsPasteRun::default();
        batch(&mut run, &editor, "prefix", Some(&clipboard), now);
        let tail = if next_clipboard == Some(clipboard.as_str()) {
            "unrelated"
        } else {
            &body
        };
        batch(
            &mut run,
            &editor,
            tail,
            next_clipboard,
            now + Duration::from_millis(5),
        );
        assert_eq!(editor.get_expanded_text(), format!("prefix{tail}"));
        assert!(
            editor.get_text().starts_with("prefix"),
            "unproven chunks must not refold the prefix"
        );
    }
}

#[test]
fn windows_split_paste_expired_or_explicitly_ended_session_does_not_merge() {
    let now = Instant::now();
    let body = "\nbody".repeat(12);
    let clipboard = format!("prefix{body}");
    for explicit in [false, true] {
        let editor = Editor::simple();
        let mut run = WindowsPasteRun::default();
        batch(&mut run, &editor, "prefix", Some(&clipboard), now);
        let at = if explicit {
            run.end_paste_session();
            now + Duration::from_millis(5)
        } else {
            now + PASTE_MATURE_CARRY_GAP + Duration::from_millis(1)
        };
        batch(&mut run, &editor, &body, Some(&clipboard), at);
        assert_eq!(editor.get_expanded_text(), clipboard);
        assert!(editor.get_text().starts_with("prefix[paste #"));
    }
}

#[test]
fn windows_split_paste_ambiguous_repeated_prefix_is_not_joined() {
    let now = Instant::now();
    let clipboard = "A".repeat(1200);
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    batch(&mut run, &editor, &clipboard[..600], Some(&clipboard), now);
    batch(
        &mut run,
        &editor,
        &clipboard[600..],
        Some(&clipboard),
        now + Duration::from_millis(5),
    );
    assert_eq!(
        editor.get_text(),
        clipboard,
        "an identical new prefix is ambiguous, not proof of continuation"
    );
}

#[test]
fn windows_split_paste_intervening_edit_keeps_the_draft_and_current_chunk() {
    let now = Instant::now();
    let body = "\nbody".repeat(12);
    let clipboard = format!("prefix{body}");
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    batch(&mut run, &editor, "prefix", Some(&clipboard), now);
    editor.set_cursor(0, 0);
    editor.insert("user edit:");
    batch(
        &mut run,
        &editor,
        &body,
        Some(&clipboard),
        now + Duration::from_millis(5),
    );
    assert_eq!(
        editor.get_expanded_text(),
        format!("user edit:{body}prefix")
    );
}

#[test]
fn windows_split_paste_ime_stays_immediate_and_never_gains_enter_evidence() {
    let now = Instant::now();
    let clipboard = format!("我们{}", "\nother".repeat(12));
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    batch(&mut run, &editor, "我们", Some(&clipboard), now);
    assert_eq!(editor.get_text(), "我们");
    assert!(!windows_enter_in_paste(
        &run,
        false,
        Some(&clipboard),
        now + Duration::from_millis(200)
    ));
    editor.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    ));
    assert_eq!(editor.get_text(), "");
}

#[test]
fn windows_split_paste_plain_typing_never_reads_the_clipboard() {
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    run.feed('我', Instant::now());
    run.flush_with_clipboard(&editor, Instant::now(), || {
        panic!("typing must not read the clipboard")
    });
    assert_eq!(editor.get_text(), "我");
}

#[test]
fn windows_split_paste_coalesced_ime_commits_do_not_read_the_clipboard() {
    let now = Instant::now();
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    for (i, word) in ["我们", "一起"].iter().enumerate() {
        let at = now + Duration::from_millis(i as u64 * 200);
        for ch in word.chars() {
            run.feed(ch, at);
        }
        run.mark_collected();
        run.flush_with_clipboard(&editor, at, || {
            panic!("coalesced IME commits must not read the clipboard")
        });
        assert!(!run.is_buffering());
    }
    assert_eq!(editor.get_text(), "我们一起");
}
