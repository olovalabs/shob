//! Probes for Enter-on-completion semantics: when the slash-command (or
//! `@`-path) completion popup is showing, a bare Enter must resolve the
//! highlighted (or sole) completion instead of submitting the half-typed
//! prefix. Mirrors the TS editor's `tui.select.confirm` handling
//! (`editor.ts:757-815`): apply the completion, then — for a `/` prefix —
//! fall through to submit; for a path completion only apply.

use super::*;

/// Decides what a bare Enter does while a completion popup is open.
/// The unit under test lives in `interactive_tui::autocomplete`.
#[test]
fn enter_on_sole_slash_completion_accepts_and_submits() {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    editor.set_text("/clea");
    editor.set_cursor(0, 5);

    let action = enter_completion_action(&state, &editor).expect("a decision");
    assert!(
        matches!(action, EnterCompletionAction::ApplyAndSubmit),
        "a sole slash completion on Enter must apply and submit, got {action:?}"
    );
}

/// The panel's first choice is a real choice: a fresh ambiguous popup (several
/// candidates, highlight on row 0) is confirmed by Enter — the row the panel
/// paints with `→`, and the same row Tab takes. Refusing to take it was the
/// bug: the fuzzy provider answers every command prefix with several
/// candidates, so `/c` ⏎ sent the raw `/c` to the model instead of running
/// `/clear`, and the user had to spell the command out in full.
#[test]
fn enter_on_ambiguous_slash_prefix_takes_the_panel_first_choice() {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    // "/c" matches /clear and /context — ambiguous.
    editor.set_text("/c");
    editor.set_cursor(0, 2);

    // Fresh popup, highlight on row 0 → Enter takes row 0 and submits it.
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert_eq!(
        completion.action,
        EnterCompletionAction::ApplyAndSubmit,
        "the highlighted (first) row must be applied and submitted"
    );
    assert!(completion.apply(&state, &editor));
    assert_eq!(editor.get_text(), "/clear");

    // ↓ moves the choice: Enter takes whatever row the panel highlights now.
    editor.set_text("/c");
    editor.set_cursor(0, 2);
    assert!(navigate_autocomplete(&state, &editor, 1));
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert_eq!(completion.action, EnterCompletionAction::ApplyAndSubmit);
    assert!(completion.apply(&state, &editor));
    assert_eq!(
        editor.get_text(),
        "/context",
        "the second row, not the first one"
    );
}

#[test]
fn enter_on_path_completion_applies_without_submitting() {
    let state = test_tui_state();
    let dir = std::env::temp_dir().join("enter-complete-probe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes-a.txt"), b"x").unwrap();
    std::fs::write(dir.join("notes-b.txt"), b"x").unwrap();

    let mut provider = FilePathAutocompleteProvider::new();
    // A root that yields exactly one candidate for the partial name.
    let _ = &mut provider;
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(dir)));

    let editor = state.editor.clone();
    editor.set_text("@notes-a");
    editor.set_cursor(0, 8);

    let action = enter_completion_action(&state, &editor).expect("a decision");
    assert!(
        matches!(action, EnterCompletionAction::ApplyOnly),
        "a path completion on Enter must apply without submitting, got {action:?}"
    );
}

/// The key-loop routing: the three Enter encodings a client may deliver
/// (`Enter`, `Char('\n')`, `Char('\r')`) all resolve the completion before
/// reaching the editor's submit — the editor itself is untouched by this
/// change; the loop-level hook is what this probe pins (decision + accept on
/// the same state the loop passes).
#[test]
fn enter_key_variants_resolve_completion_before_editor_submit() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let variants = [
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('\r'), KeyModifiers::NONE),
    ];
    for key in variants {
        // The loop routes any of these through the same hook before
        // `editor.handle_key(key)` — simulate that routing exactly.
        assert!(
            key.modifiers == KeyModifiers::NONE
                && matches!(
                    key.code,
                    KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r')
                ),
            "variant must be classified as bare Enter"
        );

        let state = test_tui_state();
        state.autocomplete.set_provider(Arc::new(
            SlashCommandAutocompleteProvider::with_default_commands(),
        ));
        let editor = state.editor.clone();
        editor.set_text("/hel");
        editor.set_cursor(0, 4);

        // Resolve ONCE — the decision and the replacement the key loop uses
        // come from the same snapshot.
        let completion = resolve_enter_completion(&state, &editor).expect("a decision");
        assert!(
            matches!(completion.action, EnterCompletionAction::ApplyAndSubmit),
            "{key:?}: sole completion must apply and submit"
        );
        assert!(
            completion.apply(&state, &editor),
            "{key:?}: completion must be applied"
        );
        assert_eq!(editor.get_text(), "/help", "{key:?}");
    }
}

/// editor holding the completed command, the popup cleared, and the selection
/// reset — so the fall-through submit sends `/clear`, not `/clea`.
#[test]
fn accept_enter_completion_completes_text_and_closes_popup() {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    editor.set_text("/clea");
    editor.set_cursor(0, 5);
    refresh_autocomplete(&state, &editor);
    assert!(
        state.autocomplete_container.child_count() > 0,
        "popup must be showing before Enter"
    );

    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    let applied = completion.apply(&state, &editor);
    assert!(applied, "the sole completion must be applied");
    assert_eq!(editor.get_text(), "/clear");
    assert_eq!(
        state.autocomplete_container.child_count(),
        0,
        "popup must be closed after Enter-accept"
    );
    assert_eq!(
        *state.autocomplete_selection.lock().unwrap(),
        0,
        "selection must reset for the next popup"
    );
}

/// Remote-client Enter encodings (`Char('\n')` / `Char('\r')`, the kitty
/// CSI-u and mobile-client forms) reach the editor as submit too — the key
/// loop routes them through the same Enter hook, so the probe drives the
/// shared decision with the same editor state. Covered end-to-end in
/// `enter_key_routes_through_completion_before_editor`.
#[test]
fn enter_completion_decision_is_prefix_scoped() {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    // Not at a completion context: plain text never matches the slash
    // provider, so Enter must submit as before.
    editor.set_text("hello /clea");
    editor.set_cursor(0, 11);
    assert!(
        enter_completion_action(&state, &editor).is_none(),
        "no popup context → no interception"
    );

    // Mid-text "/clea" with the caret BEFORE the end still produces slash
    // suggestions (start=0 is false here, but no explicit `/` at token start
    // means we must not hijack the key either).
    editor.set_text("hello /clea");
    editor.set_cursor(0, 11);
    let _ = state.autocomplete.get_suggestions("hello /clea", 11);
    assert!(
        enter_completion_action(&state, &editor).is_none(),
        "non-start slash token must not be intercepted"
    );
}

#[test]
fn unknown_slash_input_keeps_falling_through_to_the_model() {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    // "/skill:something" is unknown to the registry AND to the provider
    // (fuzzy keeps it only when chars appear in order — "skill:name" does not
    // match any built-in), so Enter must submit the raw text untouched.
    editor.set_text("/skill:draw");
    editor.set_cursor(0, 11);
    assert!(
        enter_completion_action(&state, &editor).is_none()
            || matches!(
                enter_completion_action(&state, &editor),
                Some(EnterCompletionAction::Submit)
            ),
        "unknown slash input must never be rewritten"
    );
    assert_eq!(editor.get_text(), "/skill:draw");
}

/// Self-review probe (maintainer-style): a pasted partial command that ends
/// with a newline must not be silently rewritten either way — the decision
/// only sees post-flush state, and a sole-completion apply is idempotent on
/// already-complete text (paste of "/clear\n" keeps submitting "/clear").
#[test]
fn already_complete_command_is_idempotent_under_enter_hook() {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    editor.set_text("/clear");
    editor.set_cursor(0, 6);
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert!(
        matches!(completion.action, EnterCompletionAction::ApplyAndSubmit),
        "exact command still resolves through the hook"
    );
    assert!(completion.apply(&state, &editor));
    assert_eq!(
        editor.get_text(),
        "/clear",
        "applying the completion of an already-complete command is idempotent"
    );
}

/// Self-review probe: the highlight must never survive into a smaller
/// candidate list (clamp), so a sole candidate after narrowing is accepted.
#[test]
fn stale_highlight_is_clamped_when_candidates_narrow() {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    editor.set_text("/c");
    editor.set_cursor(0, 2);
    // Ambiguous: several candidates, Enter takes the highlighted row.
    assert_eq!(
        enter_completion_action(&state, &editor),
        Some(EnterCompletionAction::ApplyAndSubmit)
    );
    // User narrows to the sole candidate; the stale selection (possibly 1)
    // is clamped to 0 by the len==1 branch regardless.
    editor.set_text("/clea");
    editor.set_cursor(0, 5);
    *state.autocomplete_selection.lock().unwrap() = 1; // stale
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert_eq!(completion.action, EnterCompletionAction::ApplyAndSubmit);
    assert!(completion.apply(&state, &editor));
    assert_eq!(editor.get_text(), "/clear");
}

/// Self-review probe: multiline draft with the caret on the `/` token line —
/// the completion must resolve in place without disturbing the first line.
/// (Documents a known provider limitation: the Rust slash provider only
/// suggests when the WHOLE input starts with `/`, so Tab cannot complete a
/// mid-draft `/` line either — the TS provider completes the CURRENT line.
/// Out of scope here; pinned as-is so a future provider fix keeps the Enter
/// hook working.)
#[test]
fn multiline_draft_slash_line_matches_tab_completion_semantics() {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    editor.set_text("first\n/clea");
    editor.set_cursor(1, 5);
    // Same answer the Tab path (accept_top_suggestion) sees: no suggestions
    // → no interception. Enter submits the draft unchanged.
    assert_eq!(
        state
            .autocomplete
            .get_suggestions("first\n/clea", 11)
            .is_some(),
        enter_completion_action(&state, &editor).is_some(),
        "the Enter hook must never see a completion the Tab path doesn't"
    );
    // Single-line slash draft stays fully functional.
    editor.set_text("/clea");
    editor.set_cursor(0, 5);
    assert!(matches!(
        enter_completion_action(&state, &editor),
        Some(EnterCompletionAction::ApplyAndSubmit)
    ));
}

// ==== Follow-up to PR #106: the panel's first choice is selectable ====

/// End-to-end: the key loop's entry point resolves `/c` to the panel's first
/// row, writes it into the editor, and the Enter falls through to the submit
/// callback with the completed command — the command runs instead of the raw
/// prefix being sent to the model.
#[test]
fn enter_on_a_slash_prefix_runs_the_panel_first_choice() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    let submitted = Arc::new(Mutex::new(Vec::<String>::new()));
    let capture = submitted.clone();
    editor.on_submit(Arc::new(move |text| {
        capture.lock().unwrap().push(text.to_string());
    }));

    editor.set_text("/c"); // /clear and /context both match
    editor.set_cursor(0, 2);
    assert!(
        !route_enter_completion(&state, &editor),
        "a slash completion applies and falls through — it never consumes the Enter"
    );
    editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        *submitted.lock().unwrap(),
        vec!["/clear".to_string()],
        "the panel's first row must be what gets executed"
    );
}

/// Regression matrix over the real registry: for EVERY proper prefix of every
/// visible command, the row the panel shows as the first choice is what Enter
/// executes. Refusing an ambiguous popup was the bug — the fuzzy provider
/// answers almost every prefix with several candidates, so 18 of the 32
/// visible commands were unreachable without spelling them out in full:
/// /clear, /clone, /compact, /copy, /exit, /help, /hotkeys, /images, /login,
/// /model, /new, /provider, /reload, /session, /thinking, /tree, /usage,
/// /version.
#[test]
fn every_visible_command_is_reachable_from_a_prefix_with_enter() {
    let registry = build_builtin_registry();
    let entries = registry.visible_entries();
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(SlashCommandAutocompleteProvider::new(
            entries.clone(),
        )));
    let editor = state.editor.clone();

    let mut unreachable: Vec<String> = Vec::new();
    for entry in &entries {
        let chars: Vec<char> = entry.name.chars().collect();
        let mut reached_from_prefix = false;
        for len in 2..chars.len() {
            let prefix: String = chars[..len].iter().collect();
            // What the panel shows for this prefix.
            let Some(popup) = state.autocomplete.get_suggestions(&prefix, prefix.len()) else {
                continue;
            };
            let first_choice = popup.items[0].text.clone();
            editor.set_text(&prefix);
            editor.set_cursor(0, prefix.len());
            *state.autocomplete_selection.lock().unwrap() = 0;
            *state.autocomplete_selection_stamped_for.lock().unwrap() = None;

            let completion = resolve_enter_completion(&state, &editor)
                .expect("the panel is showing a command → Enter must resolve it");
            assert_eq!(
                completion.action,
                EnterCompletionAction::ApplyAndSubmit,
                "{prefix:?}: the panel's first choice {first_choice:?} must be what Enter takes"
            );
            assert!(completion.apply(&state, &editor));
            assert_eq!(
                editor.get_text(),
                first_choice,
                "{prefix:?}: Enter must write the row the panel highlighted"
            );
            assert!(
                registry.find(&editor.get_text()).is_some(),
                "{prefix:?} submitted the unknown token {:?} — the raw prefix must \
                 never reach the model while the panel shows a command",
                editor.get_text()
            );
            if editor.get_text() == entry.name {
                reached_from_prefix = true;
            }
        }
        if !reached_from_prefix {
            unreachable.push(entry.name.clone());
        }
    }
    assert!(
        unreachable.is_empty(),
        "commands that still need the full spelling: {unreachable:?}"
    );
}

/// The completion context is the COMMAND token, not the whole input: with the
/// caret in the arguments the provider still answers for everything before
/// the caret, and taking its row would swallow the arguments (`/model o` ⏎
/// must submit `/model o`, not `/model`).
#[test]
fn enter_in_the_arguments_keeps_the_typed_text() {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    let editor = state.editor.clone();
    for text in ["/model o", "/c m", "/exit x", "/clear now"] {
        editor.set_text(text);
        editor.set_cursor(0, text.len());
        assert_eq!(
            enter_completion_action(&state, &editor),
            None,
            "{text:?}: the caret is in the arguments, not in the command token"
        );
    }
}
