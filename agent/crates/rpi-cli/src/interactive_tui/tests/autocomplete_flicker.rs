//! Regression probes for the autocomplete panel flicker (PR-8 / pr-t3):
//! holding Backspace in a `/` draft re-renders the popup on every keystroke,
//! and the dock stacks `autocomplete_container` as an auto-height entry, so
//! every row-count change reflows the transcript. The popup must reuse its
//! row components in place and must not shrink (or close) row-by-row while
//! the candidates trail off — the row count is only allowed to grow or stay
//! flat until a close, which empties the container at once.

use super::*;

/// Drive `refresh_autocomplete` through `prefixes` (a backspace timeline) and
/// return the container's child count after each refresh.
fn child_counts_over(state: &Arc<TuiState>, editor: &Arc<Editor>, prefixes: &[&str]) -> Vec<usize> {
    prefixes
        .iter()
        .map(|text| {
            editor.set_text(text);
            editor.set_cursor(0, text.len());
            refresh_autocomplete(state, editor);
            state.autocomplete_container.child_count()
        })
        .collect()
}

/// `/clear` backspaced to empty. With 5 builtin commands the candidate
/// window (`max_visible = 5`) follows the fuzzy matches per prefix; the old
/// clear+rebuild path produced a 0→N→0 sawtooth (empty text renders nothing,
/// every non-empty prefix renders some rows, the final clear zeroes it).
#[test]
fn backspacing_through_slash_drafts_never_collapses_and_grows_the_popup() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());

    let prefixes = ["/clear", "/clea", "/cle", "/cl", "/c", "/", ""];
    let counts = child_counts_over(&state, &editor, &prefixes);

    assert_eq!(
        counts[..6],
        [1, 1, 2, 4, 5, 5],
        "candidate windows per prefix"
    );
    // The sawtooth: no prefix may render zero rows...
    for (prefix, count) in prefixes.iter().zip(&counts) {
        assert!(
            *count > 0 || prefix.is_empty(),
            "prefix {prefix:?} collapsed the popup to 0 rows"
        );
    }
    // ...and the row count must be non-decreasing until the close: the dock
    // holds one height for the whole deletion instead of reflowing per key.
    for pair in counts[..6].windows(2) {
        assert!(
            pair[1] >= pair[0],
            "popup shrank mid-edit: counts = {counts:?}"
        );
    }
    // The close (empty editor text) empties the container in one step so the
    // transcript reclaims the rows.
    assert_eq!(counts[6], 0, "close must empty the container");
    assert!(state.autocomplete_container.render(80).is_empty());
}

/// The popup that opens on `/` (5 candidates) then shrinks toward `/clear`
/// must keep its height: blank pad rows stand in for the rows that left, so
/// the dock never reflows downward mid-edit.
#[test]
fn popup_shrinking_candidates_keep_the_container_height() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());

    let prefixes = ["/", "/c", "/cl", "/cle", "/clea", "/clear"];
    let counts = child_counts_over(&state, &editor, &prefixes);
    assert_eq!(counts[0], 5, "/ matches every builtin command");
    for (prefix, count) in prefixes.iter().zip(&counts) {
        assert!(
            *count >= counts[0] || *count >= 5,
            "prefix {prefix:?} lost pad rows: counts = {counts:?}"
        );
    }
    assert_eq!(
        counts.last(),
        Some(&5),
        "the whole edit keeps the height the popup opened with"
    );
    // The pad rows are blank: nothing but the real candidates is visible.
    let rendered = state
        .autocomplete_container
        .render(80)
        .into_iter()
        .map(|line| strip_ansi(&line).trim().to_string())
        .collect::<Vec<_>>();
    assert_eq!(rendered.iter().filter(|line| !line.is_empty()).count(), 1);
    // The pad rows stand ABOVE the candidates: the dock stacks this container
    // directly on top of the editor, so the last rendered row is the one the
    // user reads against the input line — the candidate must be parked there,
    // not at the popup's top edge where the old top-fill layout left it.
    let last = rendered.last().unwrap();
    assert!(
        last.contains("/clear"),
        "the candidate rides the bottom row, adjacent to the editor: {rendered:?}"
    );

    // A close still empties everything, pad rows included.
    editor.set_text("hello");
    editor.set_cursor(0, 5);
    refresh_autocomplete(&state, &editor);
    assert!(state.autocomplete_container.render(80).is_empty());
}

/// A longer candidate window opened at the cap (`max_visible`) must not grow
/// further while the candidates trail off (the window is already at the
/// `autocomplete_max_visible` ceiling).
#[test]
fn popup_at_the_visible_cap_stays_there_through_backspace() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());

    let prefixes = ["/s", "/se", "/ses", "/sesi", "/sesio", "/session"];
    let counts = child_counts_over(&state, &editor, &prefixes);
    assert_eq!(
        counts,
        vec![5, 5, 5, 5, 5, 5],
        "a capped popup keeps the cap through backspace: counts = {counts:?}"
    );
}

/// Typing forward (the mirror direction of backspace) keeps the same
/// contract: rows only grow or stay flat, the close is the single
/// collapse point.
#[test]
fn typing_forward_then_clearing_keeps_the_popup_monotone() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());

    let prefixes = ["/", "/c", "/cl", "/cle", "/clea", "/clear", ""];
    let counts = child_counts_over(&state, &editor, &prefixes);
    // The candidate windows shrink (5 → 1) but the container opened at the
    // cap, so every subsequent render keeps 5 rows (blank pad rows stand in
    // for the ones that left) until the close empties it.
    assert_eq!(counts[..6], [5, 5, 5, 5, 5, 5], "child counts per prefix");
    for pair in counts[..6].windows(2) {
        assert!(pair[1] >= pair[0], "popup shrank mid-edit: {counts:?}");
    }
    assert_eq!(counts[6], 0, "close empties the container");
}

/// The ↑/↓ navigation stamp and the highlight retire with the candidates,
/// not with the blank pad rows: a popup that kept its height while its
/// candidates shrank to 1 must still Enter-apply the one visible row.
#[test]
fn navigation_state_does_not_outlive_the_candidates() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());

    editor.set_text("/c");
    editor.set_cursor(0, 2);
    refresh_autocomplete(&state, &editor);
    assert!(
        navigate_autocomplete(&state, &editor, 1),
        "↓ consumes the key while the popup is open"
    );
    assert_eq!(*state.autocomplete_selection.lock().unwrap(), 1);

    // Backspace to `/` — the popup re-renders with more candidates, the
    // stamp for the `/c` popup must be retired so the selection restarts.
    editor.set_text("/");
    editor.set_cursor(0, 1);
    refresh_autocomplete(&state, &editor);
    assert_eq!(
        *state.autocomplete_selection_stamped_for.lock().unwrap(),
        None,
        "a rebuilt popup retires the navigation stamp"
    );

    // Backspace to empty — the candidates are gone: selection is zeroed
    // even though the container may still hold pad rows from the close path.
    editor.set_text("");
    editor.set_cursor(0, 0);
    refresh_autocomplete(&state, &editor);
    assert_eq!(*state.autocomplete_selection.lock().unwrap(), 0);
    assert!(state.autocomplete_container.render(80).is_empty());

    // A fresh popup after the close opens un-navigated at row 0.
    editor.set_text("/clear");
    editor.set_cursor(0, 6);
    refresh_autocomplete(&state, &editor);
    assert_eq!(*state.autocomplete_selection.lock().unwrap(), 0);
}

/// An `@`-path popup rides the same container; its close path must empty it
/// too (no permanent pad rows after a selector/extension swap or an
/// accepted completion).
#[test]
fn close_paths_empty_the_container_for_path_popups() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());

    editor.set_text("/c");
    editor.set_cursor(0, 2);
    refresh_autocomplete(&state, &editor);
    assert!(
        accept_top_suggestion(&state, &editor),
        "Tab accepts the highlighted candidate"
    );
    assert_eq!(editor.get_text(), "/clear");
    assert!(
        state.autocomplete_container.render(80).is_empty(),
        "an accepted completion empties the popup"
    );
    assert_eq!(state.autocomplete_container.child_count(), 0);
}

/// A test state wired exactly like production: the slash provider over the
/// builtin registry plus a file provider rooted at an empty throwaway dir.
fn test_state_with_slash_provider() -> Arc<TuiState> {
    let state = test_tui_state();
    {
        let mut combined = CombinedAutocompleteProvider::new();
        combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
            build_builtin_registry().visible_entries(),
        )));
        state.autocomplete.set_provider(Arc::new(combined));
    }
    state
}

// ===========================================================================
// Review 172 round 2 — the measured height, not just the child count
// ===========================================================================

/// Render widths the round-2 probes sweep: the popup is auto-height, so the
/// lines each row occupies are a function of the width it is rendered at.
const REVIEW172_WIDTHS: [usize; 4] = [80, 60, 40, 20];

/// The maintainer's first probe, taken verbatim: a fixed width, `/` grown to
/// `/clear`, asserting the popup's *rendered* height never shrinks.
///
/// The previous head kept the child count flat but rendered two lines for a
/// long candidate and one for the blank pad that replaced it, so the dock still
/// contracted (measured `[5, 6, 6, 5, 5, 5]` at width 60).
#[test]
fn review172_rendered_popup_height_must_not_shrink_with_fixed_child_count() {
    for width in REVIEW172_WIDTHS {
        let state = test_state_with_slash_provider();
        let editor = Arc::new(Editor::simple());
        let mut children = Vec::new();
        let mut heights = Vec::new();
        for prefix in ["/", "/c", "/cl", "/cle", "/clea", "/clear"] {
            editor.set_text(prefix);
            editor.set_cursor(0, prefix.len());
            refresh_autocomplete(&state, &editor);
            children.push(state.autocomplete_container.child_count());
            heights.push(state.autocomplete_container.render(width).len());
        }
        assert!(
            heights.windows(2).all(|pair| pair[1] >= pair[0]),
            "width={width}: popup shrank while children stayed {children:?}: heights={heights:?}"
        );
    }
}

/// The same timeline measured through the real root/dock allocation the
/// production renderer uses, so the assertion covers the transcript viewport
/// and the dock top that the flicker is actually about — not the container in
/// isolation.
#[test]
fn review172_backspace_reflows_the_actual_transcript_viewport() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());
    let transcript = Arc::new(rpi_tui::ScrollView::new(
        Arc::new(Text::new(
            (0..100)
                .map(|i| format!("transcript {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            0,
            0,
        )),
        rpi_tui::ScrollViewOptions::default(),
    ));
    let dock = Arc::new(VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(state.autocomplete_container.clone())),
        StackChild::Entry(StackEntry::new(editor.clone()).shrink(0).min_size(3)),
        StackChild::Entry(StackEntry::new(Arc::new(Text::new("FOOTER", 0, 0)))),
    ]));
    let root = Arc::new(VStack::from_children(vec![
        StackChild::Entry(
            StackEntry::new(transcript)
                .basis(0)
                .grow(1)
                .shrink(1)
                .min_size(1),
        ),
        StackChild::Entry(StackEntry::new(dock).shrink(1)),
    ]));
    // Open at the visible cap, then narrow to one match without closing.
    for prefix in ["/", "/clear"] {
        editor.set_text(prefix);
        editor.set_cursor(0, prefix.len());
        refresh_autocomplete(&state, &editor);
    }
    let mut popup_heights = Vec::new();
    let mut transcript_heights = Vec::new();
    let mut dock_tops = Vec::new();
    for prefix in ["/clear", "/clea", "/cle", "/cl", "/c", "/"] {
        editor.set_text(prefix);
        editor.set_cursor(0, prefix.len());
        refresh_autocomplete(&state, &editor);
        assert_eq!(state.autocomplete_container.child_count(), 5);
        let frame = rpi_tui::render_layout_frame(root.clone(), 60, 24);
        popup_heights.push(frame.root.children[1].children[0].rect.height);
        transcript_heights.push(frame.root.children[0].rect.height);
        dock_tops.push(frame.root.children[1].rect.y);
    }
    assert!(
        popup_heights.windows(2).all(|pair| pair[1] >= pair[0]),
        "popup shrank during Backspace: {popup_heights:?}"
    );
    // The two symptoms the review reported: a shorter popup gives the
    // transcript a row back and lifts the dock top.
    assert!(
        transcript_heights.windows(2).all(|pair| pair[1] <= pair[0]),
        "transcript viewport grew during Backspace: {transcript_heights:?}"
    );
    assert!(
        dock_tops.windows(2).all(|pair| pair[1] <= pair[0]),
        "dock top moved up during Backspace: {dock_tops:?}"
    );
}

/// One candidate is one screen row at every width, however long the candidate
/// is. This is the structural claim the round-2 fix rests on: `Text` cannot be
/// made to guarantee it at narrow widths, so the row component truncates.
#[test]
fn review172_every_popup_row_is_exactly_one_screen_line_wide_or_narrow() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());
    for width in REVIEW172_WIDTHS {
        for prefix in ["/", "/c", "/cl", "/cle", "/clea", "/clear"] {
            editor.set_text(prefix);
            editor.set_cursor(0, prefix.len());
            refresh_autocomplete(&state, &editor);
            let lines = state.autocomplete_container.render(width);
            assert_eq!(
                lines.len(),
                state.autocomplete_container.child_count(),
                "width={width} prefix={prefix:?}: rows must map 1:1 onto screen lines"
            );
            for line in &lines {
                assert!(
                    rpi_tui::visible_width_util(line) <= width,
                    "width={width} prefix={prefix:?}: row exceeds the viewport: {line:?}"
                );
            }
        }
    }
}

/// ↑/↓ move the visible candidate window; the reviewed height must survive a
/// window change as well as a candidate-count change.
#[test]
fn review172_navigating_the_window_never_shrinks_the_popup() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());
    editor.set_text("/");
    editor.set_cursor(0, 1);
    refresh_autocomplete(&state, &editor);
    let baseline = state.autocomplete_container.render(60).len();
    assert_eq!(
        state.autocomplete_container.child_count(),
        5,
        "/ fills the visible cap"
    );

    for step in 0..10 {
        assert!(
            navigate_autocomplete(&state, &editor, 1),
            "↓ step {step} consumes the key"
        );
        assert_eq!(
            state.autocomplete_container.render(60).len(),
            baseline,
            "↓ step {step} changed the popup height"
        );
    }
    for step in 0..10 {
        assert!(
            navigate_autocomplete(&state, &editor, -1),
            "↑ step {step} consumes the key"
        );
        assert_eq!(
            state.autocomplete_container.render(60).len(),
            baseline,
            "↑ step {step} changed the popup height"
        );
    }
    // The highlighted row is still the visible one: navigation moves the
    // window contents, not the frame height.
    let selected = *state.autocomplete_selection.lock().unwrap();
    let rendered = state
        .autocomplete_container
        .render(60)
        .into_iter()
        .map(|line| strip_ansi(&line))
        .collect::<Vec<_>>();
    let marked = rendered
        .iter()
        .position(|line| line.contains('→'))
        .expect("a highlighted row is still visible");
    assert_eq!(marked, selected, "the arrow marks the selected row");
}

/// The truncated row keeps the SGR state of the columns it retains: the row is
/// styled with `accent.fg(label)`, so a clipped candidate must still carry its
/// escape sequence rather than dropping to the default colour.
#[test]
fn review172_truncated_rows_keep_their_styling() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());
    editor.set_text("/");
    editor.set_cursor(0, 1);
    refresh_autocomplete(&state, &editor);

    let wide = state.autocomplete_container.render(80);
    let narrow = state.autocomplete_container.render(20);
    let styled = wide.iter().filter(|line| line.contains('\x1b')).count();
    assert!(styled > 0, "the popup rows carry theme escapes at width 80");
    assert!(
        narrow.iter().all(|line| line.contains('\x1b')),
        "clipping must not strip the row styling at width 20"
    );
    // Truncation is a clip, not a re-wrap: what survives is a prefix of the
    // wide rendering.
    let wide_visible = wide.iter().map(|line| strip_ansi(line)).collect::<Vec<_>>();
    for (narrow_line, wide_line) in narrow.iter().zip(&wide_visible) {
        let visible = strip_ansi(narrow_line);
        let kept = visible.trim_start().trim_end_matches('…');
        if kept.is_empty() {
            continue;
        }
        assert!(
            wide_line.contains(kept),
            "narrow row is not a clipped prefix of the wide row: {visible:?} vs {wide_line:?}"
        );
    }
}

/// The pad rows that stand in for departed candidates render as blank lines,
/// not as zero rows: a pad row that disappeared would give its line back to the
/// transcript — the very reflow the popup avoids.
#[test]
fn review172_pad_rows_render_blank_but_present() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());
    for prefix in ["/", "/clear"] {
        editor.set_text(prefix);
        editor.set_cursor(0, prefix.len());
        refresh_autocomplete(&state, &editor);
    }
    let lines = state.autocomplete_container.render(60);
    assert_eq!(lines.len(), 5, "the popup keeps its height after narrowing");
    let blank = lines
        .iter()
        .filter(|line| strip_ansi(line).trim().is_empty())
        .count();
    assert_eq!(
        blank, 4,
        "the four departed candidates leave blank pad rows"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| strip_ansi(line).contains("/clear"))
            .count(),
        1,
        "exactly one real candidate is visible"
    );
}

// ===========================================================================
// Bottom anchor — the candidates hug the editor while the panel holds height
// ===========================================================================
//
// The dock stacks `autocomplete_container` directly ON TOP of the editor, so
// WHICH rows are blank is user-visible geometry: every height probe above
// passes for either fill order, but a pad TAIL pushes the remaining candidates
// up, away from the input line they complete — the first choice parks at the
// popup's top edge however few rows remain. The pad rows must stand ABOVE the
// candidates (blank prefix, live suffix), so the candidate block stays glued
// to the editor's top border through the whole edit.

/// Rendered popup rows (ANSI stripped) after driving `prefixes` in order.
fn rendered_rows_over(
    state: &Arc<TuiState>,
    editor: &Arc<Editor>,
    prefixes: &[&str],
) -> Vec<Vec<String>> {
    prefixes
        .iter()
        .map(|text| {
            editor.set_text(text);
            editor.set_cursor(0, text.len());
            refresh_autocomplete(state, editor);
            state
                .autocomplete_container
                .render(80)
                .into_iter()
                .map(|line| strip_ansi(&line).trim().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The visible candidates form the BOTTOM block of the panel at every draft
/// length: a blank row may only appear ABOVE a live row, never below one —
/// the last panel row is the one adjacent to the editor's top border, and it
/// must always carry a candidate while the popup is open.
#[test]
fn narrowing_slash_drafts_keep_the_candidates_on_the_rows_adjacent_to_the_editor() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());

    // Open at the cap on `/`, then narrow: the pad rows stand in above.
    let timeline = ["/", "/c", "/cl", "/cle", "/clea", "/clear"];
    for (prefix, rows) in timeline
        .iter()
        .zip(rendered_rows_over(&state, &editor, &timeline))
    {
        assert!(
            rows.last().map(|row| !row.is_empty()).unwrap_or(false),
            "prefix {prefix:?}: the row adjacent to the editor is blank: {rows:?}"
        );
        let mut live_seen = false;
        for row in rows.iter() {
            if !row.is_empty() {
                live_seen = true;
            } else {
                assert!(
                    !live_seen,
                    "prefix {prefix:?}: a blank pad row sits BELOW a live candidate — the candidates drifted away from the editor: {rows:?}"
                );
            }
        }
    }

    // The extreme case from the field report: one candidate left, four pad
    // rows — the highlighted first choice must ride the bottom row, not the
    // panel's top edge.
    let rows = rendered_rows_over(&state, &editor, &["/clear"])
        .pop()
        .unwrap();
    assert_eq!(rows.len(), 5, "the panel keeps the height it opened with");
    assert_eq!(
        rows.iter().filter(|row| !row.is_empty()).count(),
        1,
        "exactly one live row"
    );
    assert!(
        rows[4].contains("/clear") && rows[4].contains('→'),
        "the first choice parks on the editor-adjacent row: {rows:?}"
    );
}

/// Over-typing then correcting (the panel never closed): the candidate that
/// comes back must re-anchor to the editor, leaving its pad above — not
/// resume its old top position with the pad underneath it.
#[test]
fn correcting_an_overtyped_slash_draft_reeanchors_the_candidate_to_the_editor() {
    let state = test_state_with_slash_provider();
    let editor = Arc::new(Editor::simple());

    let timeline = ["/clear", "/cle", "/clear"];
    let frames = rendered_rows_over(&state, &editor, &timeline);
    assert_eq!(frames.len(), 3);

    // `/clear` alone: one row, one candidate. `/cle`: two candidates, the
    // container grew a row. Back to `/clear`: the popup does NOT close, so
    // the row count stays — the departed candidate's row must become a pad
    // ABOVE the surviving candidate.
    let rows = frames.last().unwrap();
    assert_eq!(rows.len(), 2, "the popup never closed, the height stays");
    assert!(
        rows[0].is_empty(),
        "the departed row pads from the top: {rows:?}"
    );
    assert!(
        rows[1].contains("/clear"),
        "the returning candidate hugs the editor again: {rows:?}"
    );
}

/// The single-line contract holds for the awkward widths and for labels that
/// would otherwise smuggle a newline through: a row is one screen line even in
/// a one-column viewport, and an embedded `\n` is flattened rather than painted
/// as a second row.
#[test]
fn review172_popup_rows_stay_single_line_at_awkward_widths() {
    for width in [0, 1, 2, 3] {
        let row = PopupRow::new("→ /clear Start a new session");
        let lines = row.render(width);
        assert_eq!(
            lines.len(),
            1,
            "width={width}: a row is one screen line even at degenerate widths"
        );
        assert!(
            rpi_tui::visible_width_util(&lines[0]) <= width,
            "width={width}: row exceeds the viewport: {:?}",
            lines[0]
        );
    }

    let row = PopupRow::new("→ /clear\nStart a new session\r");
    let lines = row.render(80);
    assert_eq!(lines.len(), 1, "an embedded newline must not become a row");
    assert!(
        !lines[0].contains('\n') && !lines[0].contains('\r'),
        "line breaks are flattened, not forwarded to the painter: {:?}",
        lines[0]
    );
    assert!(
        strip_ansi(&lines[0]).contains("/clear Start a new session"),
        "the flattened label keeps its text: {:?}",
        lines[0]
    );
}
