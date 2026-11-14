//! Completion suggestions and editor cursor operations.

use std::any::Any;

use super::*;
use rpi_tui::truncate_to_width;

// ===========================================================================
// Autocomplete
// ===========================================================================

/// Install one command-discovery snapshot at startup and after successful
/// resource reloads. Skills expand in the harness's normal input path, not in
/// slash dispatch, so images and queued messages keep their ordinary behavior.
pub(super) fn configure_resource_autocomplete(
    autocomplete: &AutocompleteManager,
    mut commands: Vec<SlashCommandEntry>,
    resources: &rpi_harness::types::AgentHarnessResources,
    cwd: &std::path::Path,
    enable_skill_commands: bool,
) {
    let resources = crate::resource_commands::resource_command_entries(resources);
    // Builtins/extensions are already in dispatch order. Deduplicate first-wins,
    // then reserve hidden builtins and aliases against resource commands too.
    let mut seen = std::collections::BTreeSet::new();
    commands.retain(|command| seen.insert(command.name.clone()));
    seen.extend(super::builtin_reserved_names());
    let skills = if enable_skill_commands {
        resources.skills
    } else {
        Vec::new()
    };
    for (name, description) in resources.templates.into_iter().chain(skills) {
        let name = if name.starts_with('/') {
            name
        } else {
            format!("/{name}")
        };
        if seen.insert(name.clone()) {
            commands.push(SlashCommandEntry { name, description });
        }
    }
    let mut combined = CombinedAutocompleteProvider::new();
    combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(commands)));
    combined.add_provider(Arc::new(FilePathAutocompleteProvider::with_root(
        cwd.to_path_buf(),
    )));
    autocomplete.set_provider(Arc::new(combined));
}

pub(super) fn skill_commands_enabled(
    global: &crate::settings::Settings,
    project: &[crate::settings::Settings],
) -> bool {
    preferred_project_setting(project, |s| s.enable_skill_commands)
        .or(global.enable_skill_commands)
        .unwrap_or(true)
}

/// Refresh the autocomplete suggestion list from the current editor text +
/// cursor. Renders the suggestions into `autocomplete_container` (above the
/// editor) or clears it when there are none.
pub(super) fn refresh_autocomplete(state: &Arc<TuiState>, editor: &Arc<Editor>) {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    let suggestions = state.autocomplete.get_suggestions(&text, cursor);
    render_autocomplete(state, suggestions);
}

/// Convert the editor's logical `(row, byte-column)` caret into the absolute
/// byte offset expected by autocomplete providers.
pub(super) fn editor_cursor_offset(editor: &Editor, text: &str) -> usize {
    let (row, col) = editor.cursor_position();
    let mut offset = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index == row {
            return (offset + col.min(line.len())).min(text.len());
        }
        offset = offset.saturating_add(line.len() + 1);
    }
    text.len()
}

/// Restore an editor caret from an absolute byte offset after autocomplete
/// replaces a span in a multi-line draft.
pub(super) fn set_editor_cursor_offset(editor: &Editor, text: &str, offset: usize) {
    let offset = offset.min(text.len());
    let mut consumed = 0;
    for (row, line) in text.split('\n').enumerate() {
        let end = consumed + line.len();
        if offset <= end {
            editor.set_cursor(row, offset - consumed);
            return;
        }
        consumed = end + 1;
    }
    let last_row = text.bytes().filter(|byte| *byte == b'\n').count();
    editor.set_cursor(
        last_row,
        text.rsplit('\n').next().map(str::len).unwrap_or(0),
    );
}

/// Retire the explicit-selection stamp when the popup is rebuilt for a
/// different completed span (typing, backspace, another token) or torn down.
/// Called with the key of the popup being rendered NOW (`None` when closing).
fn stamp_selection_for(state: &Arc<TuiState>, key: Option<&str>) {
    let mut stamp = state.autocomplete_selection_stamped_for.lock().unwrap();
    match (key, stamp.as_ref()) {
        // Same popup key → the user's navigation is still about this popup.
        (Some(current), Some(previous)) if current == previous => {}
        // New key or closing → the navigation intent does not carry over.
        _ => *stamp = None,
    }
}

/// Stable identity of a suggestion popup: what span it completes plus the
/// candidate list it offers. A popup that completes the same span with the
/// same candidates is "the same popup" however many times it re-renders.
fn sugg_key(sugg: &AutocompleteSuggestions) -> String {
    let items = sugg
        .items
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\u{1}");
    format!("{}\u{2}{}\u{2}{}", sugg.start, sugg.end, items)
}

/// Whether the popup currently completing `sugg` is the one the user last
/// navigated — the explicit-selection stamp matches its key.
fn selection_is_explicit(state: &Arc<TuiState>, sugg: &AutocompleteSuggestions) -> bool {
    let key = sugg_key(sugg);
    state
        .autocomplete_selection_stamped_for
        .lock()
        .unwrap()
        .as_deref()
        == Some(key.as_str())
}

/// Render (or clear) the autocomplete suggestion list into the container.
///
/// The popup reuses its row components in place: each render relabels the
/// existing [`PopupRow`] children instead of `clear()` + rebuild, and the row
/// count never shrinks while the popup stays open. The dock stacks this
/// container as an auto-height entry, so its height is
/// `container.render(width).len()`: a row count that follows the candidate
/// count (`/clear` backspaced: 1 → 2 → 5 → 0) reflows the whole transcript on
/// every keystroke — the panel flicker this module exists to avoid. Growing is
/// allowed (a taller popup must show its rows); shrinking is padded with blank
/// rows until the popup closes, and closing empties the container so the
/// transcript reclaims the rows.
///
/// The pad rows stand ABOVE the live candidates, not below them: the dock
/// places this container directly on top of the editor, so a pad tail would
/// push the rows the user reads away from the input they belong to — the
/// highlighted first choice would sit at the popup's top edge and stay there
/// while the draft narrows, however close the panel once was. Blanking from
/// the top keeps the candidates anchored to the editor's top border.
///
/// Keeping the row *count* flat is not enough on its own: a row rendered by
/// `Text` word-wraps, so a long candidate costs two screen lines while the
/// blank pad that replaces it costs one, and the popup still contracts behind
/// a flat child count. Rows are therefore [`PopupRow`]s, which truncate to the
/// render width and always emit exactly one line — one candidate is one
/// screen row at every width.
///
/// The selection index and the explicit-selection stamp are retired exactly
/// when candidates are gone, so ↑/↓ and Enter semantics do not outlive the
/// popup even though its blank pad rows do.
pub(super) fn render_autocomplete(
    state: &Arc<TuiState>,
    suggestions: Option<AutocompleteSuggestions>,
) {
    let Some(sugg) = suggestions.filter(|sugg| !sugg.items.is_empty()) else {
        close_autocomplete_popup(state);
        return;
    };
    // Clamp the highlight to the full candidate list, then compute the visible
    // window (with a scroll offset) so ↓ can reach items past `max_visible`.
    let max_visible = state.autocomplete_max_visible;
    let total = sugg.items.len();
    let selection = {
        let mut sel = state.autocomplete_selection.lock().unwrap();
        *sel = (*sel).min(total.saturating_sub(1));
        *sel
    };
    let window_start = if total <= max_visible {
        0
    } else {
        selection
            .saturating_sub(max_visible / 2)
            .min(total - max_visible)
    };
    let window_end = (window_start + max_visible).min(total);
    // The rebuilt popup completes this span now; a stamp for any other span
    // belongs to a previous popup and must not survive the rebuild. The span
    // is keyed by the suggestion's own [start, end) window so typing or
    // backspacing within the token retires it, while a pure re-render (same
    // span) keeps the user's navigation.
    stamp_selection_for(state, Some(&sugg_key(&sugg)));
    // Build a compact list: the highlighted item marked with `→`, the rest `  `.
    // Cap the list so the dock doesn't swallow the transcript.
    let accent = state.theme_manager.get().colors.accent;
    let muted = state.theme_manager.get().colors.muted;
    let rows = (window_start..window_end)
        .map(|i| {
            let item = &sugg.items[i];
            let prefix = if i == selection { "→ " } else { "  " };
            let label = item.display_text();
            if i == selection {
                format!(
                    "{prefix}{} {}",
                    accent.fg(label),
                    muted.fg(item.description.as_deref().unwrap_or(""))
                )
            } else {
                format!(
                    "{prefix}{} {}",
                    muted.fg(label),
                    muted.fg(item.description.as_deref().unwrap_or(""))
                )
            }
        })
        .collect::<Vec<_>>();
    render_popup_rows(state, rows);
}

/// Close the popup: retire the navigation state and empty the container so
/// the dock reclaims its rows immediately. Also the resting state after
/// `apply_suggestion` and the selector/extension swaps.
fn close_autocomplete_popup(state: &Arc<TuiState>) {
    *state.autocomplete_selection.lock().unwrap() = 0;
    stamp_selection_for(state, None);
    state.autocomplete_container.clear();
}

/// Write `rows` into the popup container, relabeling its `Text` children in
/// place and never shrinking the row count: rows that left the candidate
/// window are blanked, not removed, so the dock keeps the height the popup
/// already had while candidates shrink. The blanks are taken from the TOP of
/// the container — the dock stacks it directly above the editor, so filling
/// from the top would push the candidates up, away from the input line, and
/// park the first choice at the popup's top edge however few rows remain.
/// Only a close (`close_autocomplete_popup`) or an outside owner
/// (selector/extension swaps clear the container) resets the row set.
fn render_popup_rows(state: &Arc<TuiState>, rows: Vec<String>) {
    let children = state.autocomplete_container.get_children();
    if children
        .iter()
        .any(|child| child.as_any().downcast_ref::<PopupRow>().is_none())
    {
        // The container only ever holds the rows this module created; a
        // foreign child or an external reset re-binds it to the popup.
        state.autocomplete_container.clear();
        return render_popup_rows(state, rows);
    }
    let pad = children.len().saturating_sub(rows.len());
    for (index, child) in children.iter().enumerate() {
        let popup_row = child
            .as_any()
            .downcast_ref::<PopupRow>()
            .expect("checked above");
        let line = if index < pad {
            String::new()
        } else {
            rows[index - pad].clone()
        };
        popup_row.set_line(line);
    }
    for row in rows.iter().skip(children.len()) {
        state
            .autocomplete_container
            .add_child(Arc::new(PopupRow::new(row.clone())));
    }
}

/// One autocomplete candidate, always exactly one screen line.
///
/// The dock allocates the popup's height from `render(width).len()`, so a row
/// that word-wraps costs two lines and makes the popup — and with it the
/// transcript viewport above it — contract as soon as that candidate leaves the
/// window. Unlike `Text` (whose wrapping is the point) this row truncates to the
/// render width with an ANSI-safe helper that keeps the SGR state of the kept
/// columns, so a wide terminal shows the whole candidate and a narrow one shows
/// a clipped prefix instead of a second line.
///
/// The line is stored as a raw string and truncated per width on render; the
/// popup re-renders on every keystroke, and `truncate_to_width` is a single
/// ANSI-aware pass, so no cache is needed to keep that cheap.
pub(super) struct PopupRow {
    line: Mutex<String>,
}

impl PopupRow {
    pub(super) fn new(line: impl Into<String>) -> Self {
        Self {
            line: Mutex::new(line.into()),
        }
    }

    pub(super) fn set_line(&self, line: impl Into<String>) {
        *self.line.lock().unwrap() = line.into();
    }

    /// Collapse anything that would start a second line. A candidate label is
    /// single-line by contract, but a provider is free to hand back text with a
    /// newline in it (a file name, a description), and an embedded `\n` would
    /// travel through `Container::render` as one string and still paint two
    /// terminal rows — the same height leak this row exists to prevent.
    fn single_line(line: &str) -> String {
        if line.contains('\n') || line.contains('\r') {
            line.replace(['\n', '\r'], " ")
        } else {
            line.to_string()
        }
    }
}

impl Component for PopupRow {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return vec![String::new()];
        }
        let line = Self::single_line(&self.line.lock().unwrap());
        // `Text::new(line, 1, 0)` rendered with one column of left padding, so
        // a row occupies the same left gutter as the label it replaces while
        // the available text width shrinks by that column. The content column
        // never drops below one: in a one-column viewport the gutter is what
        // goes, not the character.
        let content_width = width.saturating_sub(1).max(1);
        let mut rendered = truncate_to_width(&line, content_width, "…");
        if content_width < width {
            rendered.insert(0, ' ');
        }
        vec![rendered]
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Accept the highlighted autocomplete suggestion: replace `text[start..end]`
/// with the suggestion text, reposition the caret, and clear the suggestion
/// list. Returns `true` if a suggestion was accepted.
pub(super) fn accept_top_suggestion(state: &Arc<TuiState>, editor: &Arc<Editor>) -> bool {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    let Some(sugg) = state.autocomplete.get_suggestions(&text, cursor) else {
        return false;
    };
    let index = {
        let sel = *state.autocomplete_selection.lock().unwrap();
        sel.min(sugg.items.len().saturating_sub(1))
    };
    let Some(top) = sugg.items.get(index) else {
        return false;
    };
    let start = sugg.start.min(text.len());
    let end = sugg.end.min(text.len());
    if sugg.is_path {
        // The path provider returns a BASENAME (`report.txt`) but spans the
        // whole path prefix, so applying its span drops the parent
        // directories: Tab on `@/abs/nested/repo` produced `@report.txt`, a
        // reference to a different file. Replace only the basename segment
        // the caret sits in — the same span the Enter path uses — and keep
        // the text as typed when the candidate does not complete that whole
        // segment (no silent drop of what the user typed after the caret).
        let (segment_start, segment_end) = path_segment_bounds(&text, start, cursor);
        let completes_whole = state
            .autocomplete
            .get_suggestions(&text, segment_end)
            .is_some_and(|whole| whole.items.iter().any(|item| item.text == top.text));
        if !completes_whole {
            return false;
        }
        return apply_suggestion(state, editor, segment_start, segment_end, top);
    }
    apply_suggestion(state, editor, start, end, top)
}

/// Replace `text[start..end]` with `item`, keep the text after the span,
/// move the caret after the inserted text, and close the popup.
fn apply_suggestion(
    state: &Arc<TuiState>,
    editor: &Arc<Editor>,
    start: usize,
    end: usize,
    item: &AutocompleteItem,
) -> bool {
    let text = editor.get_text();
    // `start`/`end` are byte offsets on char boundaries, so the `text[..start]`
    // / `text[end..]` slices are sound for multibyte input.
    let start = start.min(text.len());
    let end = end.min(text.len());
    let mut replaced = String::with_capacity(text.len() + item.text.len());
    replaced.push_str(&text[..start]);
    replaced.push_str(&item.text);
    // Keep the text AFTER the replaced span (mid-line completion: replacing
    // `[start, end)` must not drop the rest of the line).
    replaced.push_str(&text[end..]);
    if item.insert_space && !replaced.ends_with('/') {
        replaced.push(' ');
    }
    // New caret position: after the inserted text (byte offset; the editor
    // snaps `set_cursor` to a char boundary as a safety net).
    let new_cursor = replaced.len().min(
        start
            + item.text.len()
            + if item.insert_space && !item.text.ends_with('/') {
                1
            } else {
                0
            },
    );
    editor.set_text(&replaced);
    set_editor_cursor_offset(editor, &replaced, new_cursor);
    close_autocomplete_popup(state);
    true
}

/// The end of the command token that contains `cursor`.
///
/// A command token ends at the next whitespace: the text after the caret
/// that belongs to the same token is part of it, not a separate word. With
/// the caret inside `/clear` the token is `/clear` — not the prefix `/cle`
/// followed by the word `ar`.
///
/// `cursor` is a byte offset into `text`; it is snapped to a char boundary
/// first so the slice is always sound.
fn command_token_end(text: &str, cursor: usize) -> usize {
    let cursor = snap_to_char_boundary(text, cursor);
    let rest = &text[cursor..];
    let offset = rest.find(char::is_whitespace);
    cursor + offset.unwrap_or(rest.len())
}

/// Greatest char-boundary byte index in `text` that is `<= cursor`.
fn snap_to_char_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    if text.is_char_boundary(cursor) {
        return cursor;
    }
    text.char_indices()
        .take_while(|(byte, _)| *byte < cursor)
        .last()
        .map(|(byte, _)| byte)
        .unwrap_or(0)
}

/// The byte range of the path segment (the basename) the caret sits in.
///
/// `token_start` is the byte just after the `@`; the range starts after the
/// last `/` (or `\`) between there and the caret and ends at the next path
/// separator or whitespace after it. Replacing the whole path prefix instead
/// would drop the parent directories — the path provider returns a basename
/// (`report.txt`), so `@/abs/nested/repo` would collapse to `@report.txt`,
/// a reference to a different file.
fn path_segment_bounds(text: &str, token_start: usize, cursor: usize) -> (usize, usize) {
    let cursor = snap_to_char_boundary(text, cursor);
    let token_start = token_start.min(cursor);
    let before_caret = &text[token_start..cursor];
    let start = before_caret
        .rfind(['/', '\\'])
        .map(|offset| token_start + offset + 1)
        .unwrap_or(token_start);
    let after_caret = &text[cursor..];
    let end = after_caret
        .find(|c: char| c == '/' || c == '\\' || c.is_whitespace())
        .map(|offset| cursor + offset)
        .unwrap_or(text.len());
    (start, end)
}

/// What a bare Enter does while a completion popup is open. Mirrors the TS
/// editor's `tui.select.confirm` handling (`editor.ts:757-815`): apply the
/// highlighted completion, then — for a `/` prefix — fall through to submit;
/// for an `@`-path completion only apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EnterCompletionAction {
    /// Apply the completion and let the Enter fall through to submit.
    ApplyAndSubmit,
    /// Apply the completion but keep the Enter (a path completion must not
    /// send the prompt).
    ApplyOnly,
    /// No completion to apply — submit the text exactly as typed.
    Submit,
}

/// The completion an Enter key resolves in the current popup: the action the
/// key performs plus the candidate to insert.
///
/// The providers scope their suggestions to the text BEFORE the caret, so a
/// caret inside a command (`/cle|ar`) yields the prefix `/cle` and a
/// replacement span that stops at the caret. Replacing only that span keeps
/// the token's own suffix (`ar`) and turns the valid command `/clear` into
/// the unknown text `/clearar`. The Enter path therefore resolves the
/// completion for the WHOLE token the caret is in: the replaced span covers
/// the entire token, and only a candidate that completes that whole token is
/// applied — a token that completes to nothing keeps the text as typed.
///
/// The decision and the replacement travel together in this snapshot: the
/// key loop resolves ONCE and applies the candidate it decided on, so a
/// candidate that disappears between the decision and the apply (a file
/// deleted by another process, a directory change) can never turn the
/// decided `ApplyOnly` into an editor submit of the unfinished prompt.
pub(super) struct EnterCompletion {
    /// What the Enter key does.
    pub(super) action: EnterCompletionAction,
    /// The candidate to insert and the byte span it replaces.
    pub(super) candidate: Option<(AutocompleteItem, usize, usize)>,
}

impl EnterCompletion {
    /// Write the resolved replacement into the editor. Returns `true` when a
    /// candidate was applied; a `Submit` decision carries none.
    pub(super) fn apply(&self, state: &Arc<TuiState>, editor: &Arc<Editor>) -> bool {
        let Some((item, start, end)) = self.candidate.as_ref() else {
            return false;
        };
        apply_suggestion(state, editor, *start, *end, item)
    }

    /// Whether the key loop consumes this Enter instead of forwarding it to
    /// the editor's submit. An `@`-path completion never sends the prompt —
    /// and the answer is independent of whether the replacement could be
    /// applied, so a vanished candidate cannot fall through into a submit.
    pub(super) fn consumes_enter(&self) -> bool {
        self.action == EnterCompletionAction::ApplyOnly
    }
}

/// Resolve the completion an Enter key applies in the current popup.
///
/// Returns `None` when no completion popup applies (plain text, a `/` token
/// that is not at the start of the input, or no suggestions) — the key falls
/// through to the editor unchanged, preserving the upstream behavior that an
/// unknown `/skill:name` reaches the model untouched.
///
/// A `/` popup is confirmed the way the panel presents it: Enter takes the
/// HIGHLIGHTED row — index 0 on a fresh popup, which is the row the panel
/// paints as the first choice — and then falls through to submit, exactly the
/// row Tab (`accept_top_suggestion`) takes. Ambiguity is not a reason to
/// refuse: a fuzzy provider answers every prefix of every command with
/// several candidates, so requiring a sole candidate made most commands
/// unreachable by Enter (`/cle` ⏎ sent `/cle` to the model instead of running
/// `/clear`), while the panel had already shown which command row 0 is.
///
/// An `@`-path popup is different in one respect: taking a row CONSUMES the
/// Enter instead of sending the prompt, so it still requires an unambiguous
/// choice (one candidate, or an explicit ↑/↓) before Enter takes a row.
pub(super) fn resolve_enter_completion(
    state: &Arc<TuiState>,
    editor: &Arc<Editor>,
) -> Option<EnterCompletion> {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    // The popup the user sees: the candidates for the text before the caret,
    // and the popup the explicit-selection stamp is keyed to.
    let popup = state.autocomplete.get_suggestions(&text, cursor)?;
    if popup.items.is_empty() {
        return None;
    }
    let start = popup.start.min(text.len());
    // Same explicit-context rule as `navigate_autocomplete`: only the `/`
    // token at input start or an `@`-file token counts as a popup the Enter
    // key belongs to.
    let explicit = text[start..].starts_with('/') || text[..start].ends_with('@');
    if !explicit {
        return None;
    }
    // The `/` completion context is the COMMAND token the user is typing. The
    // provider answers for the whole text before the caret (`start` 0,
    // `end` = caret), so with the caret in the ARGUMENTS (`/help me`) the
    // replacement would run to the end of the line and swallow them: Enter on
    // `/help me` must submit `/help me`, not `/help`.
    let command_end = text.find(char::is_whitespace).unwrap_or(text.len());
    if !popup.is_path && cursor > command_end {
        return None;
    }
    // Explicit selection: the user navigated THIS popup with ↑/↓. The stamp
    // (set by navigation, retired when the popup changes) is what carries the
    // intent — the row index alone cannot express "↓ then ↑ selected row 0".
    let explicit_selection = selection_is_explicit(state, &popup);
    let index = {
        let sel = *state.autocomplete_selection.lock().unwrap();
        sel.min(popup.items.len().saturating_sub(1))
    };
    let Some(top) = popup.items.get(index) else {
        return None;
    };
    // A path completion consumes the Enter, so an ambiguous path popup keeps
    // plain submit until the user picks a row.
    if popup.is_path && popup.items.len() > 1 && !explicit_selection {
        return Some(EnterCompletion {
            action: EnterCompletionAction::Submit,
            candidate: None,
        });
    }
    // A candidate only rewrites text when it completes the whole unit the
    // caret sits in — the entire command token, or the entire path segment.
    // Otherwise the text stays exactly as typed: no silent rewrite to a
    // different command, and no silent drop of what the user typed after the
    // caret. An explicit ↑/↓ selection wins over that inference.
    let (replacement_start, replacement_end, completes_whole) = if popup.is_path {
        // The path provider returns a BASENAME (`report.txt`) but spans the
        // whole path prefix; replacing that span would drop the directory and
        // point the reference at a different file. Replace only the basename
        // segment, keeping the parent directories the user typed.
        let (segment_start, segment_end) = path_segment_bounds(&text, start, cursor);
        let whole = state
            .autocomplete
            .get_suggestions(&text, segment_end)
            .is_some_and(|whole| whole.items.iter().any(|item| item.text == top.text));
        (segment_start, segment_end, whole)
    } else {
        // The replacement must cover the WHOLE command token the caret sits
        // in: the text after the caret that belongs to the same token is part
        // of it, not a separate word. `/cle|ar` is the token `/clear`, so the
        // token's own suffix is replaced instead of being left behind next to
        // the completed prefix (which turned the valid command `/clear` into
        // the unknown text `/clearar`).
        let token_end = command_token_end(&text, cursor).min(text.len());
        let whole = state
            .autocomplete
            .get_suggestions(&text, token_end)
            .is_some_and(|whole| whole.items.iter().any(|item| item.text == top.text));
        (start, token_end, whole)
    };
    if !completes_whole && !explicit_selection {
        return Some(EnterCompletion {
            action: EnterCompletionAction::Submit,
            candidate: None,
        });
    }
    Some(EnterCompletion {
        action: if popup.is_path {
            EnterCompletionAction::ApplyOnly
        } else {
            EnterCompletionAction::ApplyAndSubmit
        },
        candidate: Some((top.clone(), replacement_start, replacement_end)),
    })
}

/// Route a bare Enter through the completion popup: resolve the decision ONCE,
/// apply the candidate that decision carries, and report whether the key is
/// consumed (`true` — an `@`-path completion) or must fall through to the
/// editor's submit (`false`).
///
/// Resolving once is load-bearing: the action and the replacement come from
/// the same snapshot, so the key loop can never apply a different candidate
/// than the one it decided on, and an `ApplyOnly` decision stays consumed even
/// if its replacement could not be written.
pub(super) fn route_enter_completion(state: &Arc<TuiState>, editor: &Arc<Editor>) -> bool {
    let Some(completion) = resolve_enter_completion(state, editor) else {
        return false;
    };
    completion.apply(state, editor);
    completion.consumes_enter()
}

/// Test-only projection of the decision (the action half of the snapshot).
/// Production never resolves the action separately from the candidate — it
/// goes through `route_enter_completion`, which applies the snapshot it just
/// decided on.
#[cfg(test)]
pub(super) fn enter_completion_action(
    state: &Arc<TuiState>,
    editor: &Arc<Editor>,
) -> Option<EnterCompletionAction> {
    resolve_enter_completion(state, editor).map(|completion| completion.action)
}

/// Move the autocomplete highlight up (`direction < 0`) or down (`direction > 0`)
/// and re-render the list. Returns `true` when the key was consumed.
///
/// Only takes over the arrow keys in an explicit completion context (`/` or
/// `@` prefix); a bare-word path completion (e.g. a draft containing `docs`)
/// must NOT swallow ↑/↓, which the editor uses for cursor/history movement.
pub(super) fn navigate_autocomplete(
    state: &Arc<TuiState>,
    editor: &Arc<Editor>,
    direction: i32,
) -> bool {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    let Some(sugg) = state.autocomplete.get_suggestions(&text, cursor) else {
        return false;
    };
    if sugg.items.is_empty() {
        return false;
    }
    let start = sugg.start.min(text.len());
    // `/` commands keep the slash at `start`; the path provider keeps `@` by
    // placing `start` *after* it, so look just before `start` for `@`.
    let explicit = text[start..].starts_with('/') || text[..start].ends_with('@');
    if !explicit {
        return false;
    }
    let total = sugg.items.len();
    {
        let mut sel = state.autocomplete_selection.lock().unwrap();
        if direction < 0 {
            *sel = sel.saturating_sub(1);
        } else {
            *sel = (*sel + 1).min(total.saturating_sub(1));
        }
    }
    // This popup has now been explicitly navigated: stamp it so a later Enter
    // accepts the (possibly zeroth) highlighted row as a deliberate choice.
    *state.autocomplete_selection_stamped_for.lock().unwrap() = Some(sugg_key(&sugg));
    render_autocomplete(state, Some(sugg));
    true
}
