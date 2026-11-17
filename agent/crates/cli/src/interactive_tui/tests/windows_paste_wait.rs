//! Idle-window regressions: text stays out of the editor until collected.
use super::*;

fn feed_text(run: &mut WindowsPasteRun, text: &str, at: Instant) {
    for ch in text.chars() {
        run.feed(ch, at);
    }
    if text.chars().count() > 1 {
        run.mark_collected();
    }
}

#[test]
fn windows_wait_is_15ms_for_one_character_and_120ms_for_a_burst() {
    let now = Instant::now();
    let mut run = WindowsPasteRun::default();
    assert_eq!(run.remaining_delay(now), None);
    run.feed('a', now);
    assert_eq!(run.grace(), Duration::from_millis(15));
    assert_eq!(
        run.remaining_delay(now + Duration::from_millis(14)),
        Some(Duration::from_millis(1))
    );
    assert_eq!(
        run.remaining_delay(now + Duration::from_millis(15)),
        Some(Duration::ZERO)
    );
    run.feed('b', now + Duration::from_millis(5));
    assert_eq!(run.grace(), Duration::from_millis(120));
    assert_eq!(
        run.remaining_delay(now + Duration::from_millis(124)),
        Some(Duration::from_millis(1))
    );
    assert_eq!(
        run.remaining_delay(now + Duration::from_millis(125)),
        Some(Duration::ZERO)
    );
}

#[test]
fn windows_wait_deadline_moves_only_when_text_arrives() {
    let now = Instant::now();
    let mut run = WindowsPasteRun::default();
    feed_text(&mut run, "prefix", now);
    for ms in [10, 40, 70, 100] {
        // Polling or non-text console traffic does not refresh last_at.
        assert_eq!(
            run.remaining_delay(now + Duration::from_millis(ms)),
            Some(Duration::from_millis(120 - ms))
        );
    }
    run.feed('x', now + Duration::from_millis(110));
    assert_eq!(
        run.remaining_delay(now + Duration::from_millis(120)),
        Some(Duration::from_millis(110))
    );
    assert_eq!(
        run.remaining_delay(now + Duration::from_millis(230)),
        Some(Duration::ZERO)
    );
    assert_eq!(
        run.remaining_delay(now + Duration::from_millis(300)),
        Some(Duration::ZERO)
    );
}

#[test]
fn windows_wait_shows_only_the_final_folded_paste() {
    let now = Instant::now();
    let body = (0..30)
        .map(|i| format!("\nrow {i}: 中文🙂"))
        .collect::<String>();
    let chunks = ["interactive", " ", "T", "U", "I", &body];
    let clipboard = chunks.concat();
    let editor = Editor::simple();
    editor.set_text("draft: ");
    editor.set_cursor(0, "draft: ".len());
    let mut run = WindowsPasteRun::default();
    for (i, chunk) in chunks.iter().enumerate() {
        let at = now + Duration::from_millis(i as u64 * 5);
        if run.is_buffering() {
            assert_ne!(run.remaining_delay(at), Some(Duration::ZERO));
        }
        feed_text(&mut run, chunk, at);
        assert_eq!(
            editor.get_text(),
            "draft: ",
            "no expanded intermediate frame"
        );
    }
    let deadline = now + Duration::from_millis(145);
    assert_eq!(run.remaining_delay(deadline), Some(Duration::ZERO));
    run.flush_with_clipboard(&editor, deadline, || Some(clipboard.clone()));
    assert_eq!(editor.get_text(), "draft: [paste #1 +31 lines]");
    assert_eq!(editor.get_expanded_text(), format!("draft: {clipboard}"));
    assert_eq!(run.remaining_delay(deadline), None);
    editor.undo();
    assert_eq!(editor.get_text(), "draft: ");
    editor.redo();
    assert_eq!(editor.get_expanded_text(), format!("draft: {clipboard}"));
}

#[test]
fn windows_wait_ime_enter_before_timeout_submits_instead_of_becoming_newline() {
    let now = Instant::now();
    let clipboard = format!("我们\n{}", "other\n".repeat(12));
    let editor = Editor::simple();
    let submitted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured = Arc::clone(&submitted);
    editor.on_submit(Arc::new(move |text| {
        captured.lock().unwrap().push(text.to_string())
    }));
    let mut run = WindowsPasteRun::default();
    feed_text(&mut run, "我们", now);
    let at = now + Duration::from_millis(50);
    assert_eq!(editor.get_text(), "");
    assert!(!windows_enter_in_paste(&run, false, Some(&clipboard), at));
    run.flush_with_clipboard(&editor, at, || Some(clipboard));
    run.end_paste_session();
    editor.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    ));
    assert_eq!(*submitted.lock().unwrap(), ["我们"]);
    assert_eq!(editor.get_text(), "");
}

#[test]
fn windows_wait_ime_evidence_does_not_become_fresh_at_the_display_deadline() {
    let now = Instant::now();
    let clipboard = format!("我们\n{}", "other\n".repeat(12));
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    feed_text(&mut run, "我们", now);
    run.flush_with_clipboard(&editor, now + Duration::from_millis(120), || {
        Some(clipboard.clone())
    });
    assert_eq!(editor.get_text(), "我们");
    assert_eq!(run.tail.as_ref().unwrap().at, now);
    assert!(!windows_enter_in_paste(
        &run,
        false,
        Some(&clipboard),
        now + Duration::from_millis(121)
    ));
}

#[test]
fn windows_wait_preserves_real_paste_newlines_and_later_enter_submission() {
    let now = Instant::now();
    let text = format!("head{}", "\nline 中文🙂".repeat(20));
    let clipboard = format!("{text}\n");
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    feed_text(&mut run, &text, now);
    let trailing = now + Duration::from_millis(80);
    assert!(windows_enter_in_paste(
        &run,
        false,
        Some(&clipboard),
        trailing
    ));
    run.feed('\n', trailing);
    let deadline = trailing + Duration::from_millis(120);
    run.flush_with_clipboard(&editor, deadline, || Some(clipboard.clone()));
    assert_eq!(editor.get_text(), "[paste #1 +22 lines]");
    assert_eq!(editor.get_expanded_text(), clipboard);
    assert!(!windows_enter_in_paste(
        &run,
        false,
        Some(&clipboard),
        deadline + Duration::from_millis(5)
    ));
}

#[test]
fn windows_wait_shortcut_barriers_preserve_text_order_and_skip_releases() {
    use crossterm::event::{KeyEvent, KeyEventKind};
    for code in [
        KeyCode::Backspace,
        KeyCode::Left,
        KeyCode::Tab,
        KeyCode::Esc,
    ] {
        assert!(windows_paste_key_barrier(&KeyEvent::new(
            code,
            KeyModifiers::NONE
        )));
    }
    for ch in ['c', 'v', 'z', 'g'] {
        assert!(windows_paste_key_barrier(&KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::CONTROL
        )));
    }
    assert!(windows_paste_key_barrier(&KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::SHIFT
    )));
    assert!(!windows_paste_key_barrier(&KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE
    )));
    assert!(!windows_paste_key_barrier(&KeyEvent::new(
        KeyCode::Char('中'),
        KeyModifiers::NONE
    )));
    assert!(!windows_paste_key_barrier(&KeyEvent::new(
        KeyCode::Char('A'),
        KeyModifiers::SHIFT
    )));
    let mut release = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
    release.kind = KeyEventKind::Release;
    assert!(!windows_paste_key_barrier(&release));

    let now = Instant::now();
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    feed_text(&mut run, "abc", now);
    let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
    assert!(windows_paste_key_barrier(&backspace));
    run.flush_with_clipboard(&editor, now + Duration::from_millis(5), || None);
    run.end_paste_session();
    editor.handle_key(backspace);
    assert_eq!(editor.get_text(), "ab");
    assert_eq!(run.remaining_delay(now + Duration::from_millis(200)), None);
    editor.set_text(""); // The same barrier before Ctrl+C leaves no hidden text.
    assert!(!run.is_buffering());
}
