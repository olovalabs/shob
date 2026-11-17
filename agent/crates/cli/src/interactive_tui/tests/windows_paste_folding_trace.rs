//! Real console batch sizes/timings, replayed with synthetic (non-user) text.
use super::*;

#[test]
fn windows_split_paste_replays_recorded_single_scalar_queue_gaps() {
    // 2026-10-31 Windows reproduction: 86 small batches, followed by a large
    // body 200ms later. No clipboard or user text is recorded in this fixture.
    // (elapsed ms, UTF-8 bytes, Unicode scalars, logical lines)
    let shape: &[(u64, usize, usize, usize)] = &[
        (0, 11, 11, 1),
        (1, 1, 1, 1),
        (4, 1, 1, 1),
        (4, 1, 1, 1),
        (7, 34, 33, 3),
        (7, 1, 1, 1),
        (10, 27, 27, 1),
        (10, 1, 1, 1),
        (12, 15, 15, 2),
        (12, 1, 1, 1),
        (13, 13, 12, 1),
        (13, 1, 1, 1),
        (13, 4, 4, 1),
        (14, 1, 1, 1),
        (14, 1, 1, 1),
        (15, 16, 15, 1),
        (16, 1, 1, 1),
        (16, 3, 3, 1),
        (16, 1, 1, 1),
        (17, 1, 1, 1),
        (17, 10, 9, 1),
        (18, 1, 1, 1),
        (19, 20, 19, 3),
        (19, 1, 1, 1),
        (20, 5, 5, 1),
        (20, 1, 1, 1),
        (21, 2, 2, 1),
        (21, 1, 1, 1),
        (25, 59, 53, 1),
        (25, 1, 1, 1),
        (27, 17, 16, 1),
        (27, 1, 1, 1),
        (28, 19, 18, 1),
        (29, 1, 1, 1),
        (30, 20, 19, 1),
        (31, 1, 1, 1),
        (32, 20, 19, 1),
        (33, 1, 1, 1),
        (34, 19, 18, 1),
        (35, 1, 1, 1),
        (36, 20, 19, 1),
        (36, 1, 1, 1),
        (38, 17, 16, 1),
        (38, 1, 1, 1),
        (40, 19, 18, 1),
        (40, 1, 1, 1),
        (42, 32, 29, 2),
        (43, 1, 1, 1),
        (43, 7, 7, 1),
        (44, 1, 1, 1),
        (45, 18, 17, 1),
        (45, 1, 1, 1),
        (47, 17, 16, 1),
        (47, 1, 1, 1),
        (48, 18, 17, 1),
        (49, 1, 1, 1),
        (50, 13, 12, 1),
        (51, 1, 1, 1),
        (52, 11, 10, 1),
        (52, 1, 1, 1),
        (53, 14, 13, 1),
        (53, 1, 1, 1),
        (54, 11, 10, 1),
        (55, 1, 1, 1),
        (56, 11, 10, 1),
        (56, 1, 1, 1),
        (60, 63, 58, 1),
        (61, 1, 1, 1),
        (61, 4, 4, 1),
        (61, 1, 1, 1),
        (63, 16, 15, 1),
        (63, 1, 1, 1),
        (64, 4, 4, 1),
        (64, 1, 1, 1),
        (65, 12, 11, 1),
        (65, 1, 1, 1),
        (66, 14, 13, 2),
        (67, 1, 1, 1),
        (68, 18, 17, 1),
        (69, 1, 1, 1),
        (72, 39, 36, 2),
        (72, 1, 1, 1),
        (73, 6, 6, 1),
        (73, 1, 1, 1),
        (73, 1, 1, 1),
        (73, 1, 1, 1),
        (273, 10684, 3972, 44),
    ];
    let chunks: Vec<String> = shape
        .iter()
        .enumerate()
        .map(|(i, &(_, bytes, chars, lines))| {
            if i == 0 {
                return "interactive".into();
            }
            if chars == 1 {
                // Also exercise ambiguous scalars matching the clipboard's start.
                return "i".into();
            }
            let extra = bytes - chars;
            let suffix = format!(
                "{}{}{}",
                "中".repeat(extra / 2),
                "·".repeat(extra % 2),
                "\n".repeat(lines - 1)
            );
            format!("{}{}", "x".repeat(chars - suffix.chars().count()), suffix)
        })
        .collect();
    let clipboard = chunks.concat();
    assert_eq!(clipboard.len(), 11428);
    let editor = Editor::simple();
    editor.set_text("draft: ");
    editor.set_cursor(0, "draft: ".len());
    let mut run = WindowsPasteRun::default();
    let now = Instant::now();
    let mut received = String::from("draft: ");
    for (i, chunk) in chunks.iter().enumerate() {
        let (ms, bytes, chars, lines) = shape[i];
        assert_eq!(
            (
                chunk.len(),
                chunk.chars().count(),
                chunk.split('\n').count()
            ),
            (bytes, chars, lines)
        );
        batch(
            &mut run,
            &editor,
            chunk,
            Some(&clipboard),
            now + Duration::from_millis(ms),
        );
        received.push_str(chunk);
        assert_eq!(
            editor.get_expanded_text(),
            received,
            "batch {i}: no missing, duplicate, or hidden unreceived text"
        );
    }
    assert_eq!(
        editor.get_text(),
        format!("draft: [paste #1 +{} lines]", clipboard.split('\n').count())
    );
    editor.undo();
    assert_eq!(editor.get_text(), "draft: ");
    editor.redo();
    assert_eq!(editor.get_expanded_text(), received);
}

#[test]
fn windows_split_paste_fragmented_ime_commits_never_read_clipboard_or_fold() {
    let now = Instant::now();
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    for (i, text) in ["我们", "一", "起", "测", "试"].iter().enumerate() {
        let at = now + Duration::from_millis(i as u64);
        for ch in text.chars() {
            run.feed(ch, at);
        }
        if text.chars().count() > 1 {
            run.mark_collected();
        }
        run.flush_with_clipboard(&editor, at, || {
            panic!("short IME fragments must not read clipboard")
        });
        assert!(!run.is_buffering());
        assert!(!editor.get_text().contains("[paste #"));
    }
    assert_eq!(editor.get_text(), "我们一起测试");
    assert!(!run.has_paste_evidence(now + Duration::from_millis(5)));
}

#[test]
fn windows_split_paste_ambiguous_scalar_stays_visible_until_disambiguated() {
    let now = Instant::now();
    let first = format!("intro{}", "\nbody".repeat(12));
    let clipboard = format!("{first}i!tail");
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    batch(&mut run, &editor, &first, Some(&clipboard), now);
    batch(
        &mut run,
        &editor,
        "i",
        Some(&clipboard),
        now + Duration::from_millis(1),
    );
    assert_eq!(
        editor.get_text(),
        "[paste #1 +13 lines]i",
        "ambiguous text is not yet hidden in the existing marker"
    );
    batch(
        &mut run,
        &editor,
        "!tail",
        Some(&clipboard),
        now + Duration::from_millis(2),
    );
    assert_eq!(editor.get_text(), "[paste #1 +13 lines]");
    assert_eq!(editor.get_expanded_text(), clipboard);
    editor.undo();
    assert_eq!(editor.get_text(), "");
    editor.redo();
    assert_eq!(editor.get_expanded_text(), clipboard);
}

#[test]
fn windows_split_paste_edit_after_provisional_suffix_prevents_rewriting_it() {
    let now = Instant::now();
    let first = format!("intro{}", "\nbody".repeat(12));
    let clipboard = format!("{first}i!tail");
    let editor = Editor::simple();
    let mut run = WindowsPasteRun::default();
    batch(&mut run, &editor, &first, Some(&clipboard), now);
    batch(
        &mut run,
        &editor,
        "i",
        Some(&clipboard),
        now + Duration::from_millis(1),
    );
    editor.set_cursor(0, 0);
    editor.insert("user:");
    batch(
        &mut run,
        &editor,
        "!tail",
        Some(&clipboard),
        now + Duration::from_millis(2),
    );
    assert_eq!(editor.get_expanded_text(), format!("user:!tail{first}i"));
}
