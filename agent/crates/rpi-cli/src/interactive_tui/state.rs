//! Shared TUI state, keyboard handling, input history, and auto-send scheduling.

use super::*;

// ===========================================================================
// Streaming run status
// ===========================================================================

/// The live status of the agent run, fed to the footer + status slot.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RunStatus {
    Idle,
    Working,
    Aborting,
}

/// Steps of the interactive "Add provider" form (`/provider` → Add). Each step
/// is answered through the editor (submitted with Enter).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum ProviderFormStep {
    #[default]
    Id,
    Api,
    BaseUrl,
    ApiKey,
    ModelId,
    ModelName,
}

/// In-progress state for the interactive "Add provider" form.
#[derive(Clone, Debug, Default)]
pub(super) struct ProviderFormState {
    pub(super) step: ProviderFormStep,
    pub(super) id: String,
    pub(super) api: String,
    pub(super) base_url: String,
    pub(super) api_key: String,
    pub(super) model_id: String,
    pub(super) model_name: String,
}

/// Which selector overlay (if any) is currently swapped into the editor slot.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectorKind {
    /// `/context` — read-only loaded resources; dismiss without changing chat.
    Context,
    /// `/model` — available models (live switch via `lane.set_model`).
    Model,
    /// `/thinking` — supported thinking levels (live via `lane.set_thinking_level`).
    Thinking,
    /// `/tools` — toggle builtin tools on/off.
    Tools,
    /// `/images` — toggle inline image rendering.
    Images,
    /// `/session` — browse and switch saved JSONL sessions.
    Session,
    /// `/theme` — dark / light / monochrome presets applied live.
    Theme,
    /// `/provider` — switch the live provider and persist the successful choice.
    Provider,
    /// `/scoped-models` — multi-toggle Ctrl+P cycle scope.
    ScopedModels,
    /// `/settings` — interactive settings menu (and its sub-selectors).
    Settings,
    /// `/tree` — navigate to an existing entry in the current session.
    Tree,
    /// Extension-provided selector; uses the same keyboard contract.
    Extension,
}

/// The component receiving keys while a selector is open.
///
/// Plain selectors route straight to the [`SelectList`]; searchable ones route
/// through [`SearchableSelectList`] so typing filters instead of being ignored.
/// Both variants expose the underlying list for callback registration and
/// selection inspection.
#[derive(Clone)]
pub(super) enum SelectorView {
    /// A bare `SelectList` (no search box).
    List(Arc<SelectList>),
    /// A `SelectList` wrapped with a fuzzy-filter input.
    Searchable(Arc<SearchableSelectList>),
    /// The `/settings` menu: value rows cycle in place, submenu rows open a
    /// nested selector.
    Settings(Arc<SettingsList>),
}

impl SelectorView {
    /// Route a key to the appropriate child.
    pub(super) fn handle_key(&self, key: KeyEvent) {
        match self {
            Self::List(list) => list.handle_key(key),
            Self::Searchable(searchable) => searchable.handle_key(key),
            Self::Settings(settings) => settings.handle_key(key),
        }
    }
}

/// Shared mutable TUI state, `Arc`-cloned into the drain task, the key loop,
/// and the render-tick task.
/// A draft injected by an extension via `SetEditorText` with `autoSendMs`:
/// the TUI places it in the prompt editor and submits it once `deadline`
/// passes, *unless the user touches the draft first*.
///
/// "Touching" is either a keystroke (the key loop clears this — the pressed
/// key is still dispatched to the editor, so the user simply keeps typing) or
/// any difference between the live editor text and `text` (the render tick's
/// safety net for programmatic/`Paste` changes that bypass the key path).
#[derive(Debug, Clone)]
pub(super) struct AutoSendPending {
    /// When an untouched draft is submitted.
    pub(super) deadline: std::time::Instant,
    /// The text as injected; a mismatch means the user edited it.
    pub(super) text: String,
}

/// What one tick of the auto-send countdown should do. Split out of
/// [`poll_auto_send`] so the submit/cancel rules are unit-testable without a
/// live TUI.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum AutoSendStep {
    /// No countdown is running.
    Idle,
    /// Abandon the countdown and leave the draft alone: the user edited it, or
    /// a run started underneath us.
    Cancel,
    /// The countdown elapsed untouched — submit the draft.
    Submit,
    /// Still counting down; the caller renders the remaining time.
    Wait(std::time::Duration),
}

/// Pure decision for one tick of the auto-send countdown.
pub(super) fn auto_send_step(
    pending: Option<&AutoSendPending>,
    current_text: &str,
    idle: bool,
    now: std::time::Instant,
) -> AutoSendStep {
    let Some(pending) = pending else {
        return AutoSendStep::Idle;
    };
    // Any divergence from the injected text is the user's own edit; an emptied
    // editor (a slash command ran, Esc cleared it) has nothing left to send.
    if current_text != pending.text || current_text.trim().is_empty() {
        return AutoSendStep::Cancel;
    }
    if !idle {
        return AutoSendStep::Cancel;
    }
    let remaining = pending.deadline.saturating_duration_since(now);
    if remaining.is_zero() {
        AutoSendStep::Submit
    } else {
        AutoSendStep::Wait(remaining)
    }
}

pub(super) struct TuiState {
    pub(super) history_page: history::HistoryUi,
    /// The root selected at startup, retained across session switches/reloads.
    pub(super) session_dir: std::path::PathBuf,
    /// The in-flight streaming assistant message (cleared on finalize).
    /// `Arc`-shared with the [`TranscriptView`] the event drain renders through,
    /// so the render-tick / reload / toggle paths keep direct access.
    pub(super) current_assistant: Arc<std::sync::Mutex<Option<Arc<AssistantMessageComponent>>>>,
    /// Tool-execution components keyed by `tool_call_id` (shared with the view).
    pub(super) tool_components: Arc<std::sync::Mutex<HashMap<String, Arc<ToolExecutionComponent>>>>,
    /// Bash-execution components keyed by `tool_call_id` (kept separate from the
    /// generic tool map so bash output streams into a `BashExecutionComponent`
    /// rather than a plain `ToolExecutionComponent`). Phase 5 routing.
    pub(super) bash_components: Arc<std::sync::Mutex<HashMap<String, Arc<BashExecutionComponent>>>>,
    /// Whether package/custom themes may be selected in this session.
    /// Persisted display preference toggled by Ctrl+T (shared with the view).
    pub(super) hide_thinking: Arc<std::sync::Mutex<bool>>,
    /// Global tool-output expansion preference toggled by Ctrl+O (shared with
    /// the view).
    pub(super) tool_outputs_expanded: Arc<std::sync::Mutex<bool>>,
    /// Whether the native-style terminal progress indicator is enabled.
    pub(super) show_terminal_progress: bool,
    /// Run status for the status indicator + interrupt routing.
    pub(super) status: std::sync::Mutex<RunStatus>,
    /// Extension status registry (`SetStatus` runtime action). The render tick
    /// repaints the footer only when `ext_status_revision` moved.
    pub(super) ext_status: rpi_extensions::ExtensionStatusMailbox,
    /// Revision of `ext_status` as of the last footer write.
    pub(super) ext_status_revision: std::sync::atomic::AtomicU64,
    /// Session editor-text queue (`SetEditorText`, runtime action 19). The
    /// render tick drains it into the prompt editor; a voice plugin drops a
    /// transcription in as an editable draft rather than sending it.
    pub(super) editor_text: rpi_extensions::EditorTextMailbox,
    /// Pending auto-send for an injected draft (see [`AutoSendPending`]).
    /// `None` when no countdown is running.
    pub(super) auto_send: std::sync::Mutex<Option<AutoSendPending>>,
    /// The editor text as of the last tick, used to detect `EditorChange`
    /// events (see [`TuiState::sync_editor_change`]).
    pub(super) last_editor_text: std::sync::Mutex<String>,
    /// Set by the TUI itself right before it writes the editor (an extension
    /// draft injection, the auto-send clear). The next `EditorChange` is then
    /// attributed to `extension` rather than to the user — without this a
    /// plugin could not tell its own text from a human typing.
    pub(super) programmatic_editor_write: std::sync::atomic::AtomicBool,
    /// Cancellation signal for the short phase that starts the persistent JS
    /// host and runs `before_agent_start`. The key thread can trigger this
    /// directly while the async message loop is awaiting the blocking worker.
    pub(super) js_preparation_cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// Cancellation signal for a user-initiated `!command` / `!!command` shell
    /// run. `Some` while the command executes; Esc/Ctrl+C cancels it. `None`
    /// when no user bash is running.
    pub(super) user_bash_cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// `bashExecution` messages produced by `!command` while an agent run was
    /// in flight. Flushed to the lane on `AgentEnd` so transcript order matches
    /// upstream's `_pendingBashMessages` (never spliced mid-turn).
    pub(super) pending_bash_messages: std::sync::Mutex<Vec<AgentMessage>>,
    /// The footer, updated live by the drain task.
    pub(super) footer: Arc<FooterComponent>,
    /// The status-container (status slot in the dock) — cleared/filled with a
    /// loader while a run is active.
    pub(super) status_container: Arc<Container>,
    /// The chat transcript container.
    pub(super) chat_container: Arc<Container>,
    /// The active loader shown while `Working`.
    pub(super) loader: Arc<Loader>,
    /// The editor, so status changes can drive its top-border working
    /// indicator (the spinner + elapsed time shown inside the input box border).
    pub(super) editor: Arc<Editor>,
    /// The last finalized assistant text (for `/copy`). Updated by the drain
    /// task on `MessageEnd` / `AgentEnd`.
    pub(super) last_assistant_text: std::sync::Mutex<String>,
    /// The active selector overlay, swapped into the editor slot. `Some` while
    /// a selector is open; the key loop routes to it first and restores the
    /// editor on done/cancel.
    pub(super) active_selector: std::sync::Mutex<Option<(SelectorView, SelectorKind)>>,
    /// In-progress interactive "Add provider" form (`/provider` → Add new
    /// provider). `Some` while the form is collecting fields; each step shows
    /// its own single-line input (see [`Self::active_provider_input`]) instead
    /// of reusing the chat editor.
    pub(super) provider_form: std::sync::Mutex<Option<ProviderFormState>>,
    /// The single-line input currently occupying the input slot for the active
    /// provider form step. `Some` while the form is collecting a field; the key
    /// loop routes keys to it and Enter/Esc drive the form.
    pub(super) active_provider_input: std::sync::Mutex<Option<Arc<Input>>>,
    /// The currently active OAuth flow id, or zero when no OAuth flow owns the input slot.
    /// Results carrying an older id are ignored after cancellation or a new flow.
    pub(super) provider_auth_flow_id: std::sync::atomic::AtomicU64,
    /// Monotonic source for OAuth flow ids. Kept separate so an inactive flow
    /// can use zero without allowing a late result to match a future flow.
    pub(super) provider_auth_generation: std::sync::atomic::AtomicU64,
    /// Cancellation for the provider OAuth call in flight. Created when a flow
    /// begins and cancelled when it ends (Esc, a finished result, or a new flow),
    /// so a plugin's blocking begin, exchange or revoke can stop early.
    pub(super) provider_auth_cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// The select-list currently occupying the input slot for a choice-style
    /// provider form step (the protocol step). Mutually exclusive with
    /// [`Self::active_provider_input`].
    pub(super) active_provider_select: std::sync::Mutex<Option<Arc<SelectList>>>,
    /// Extension-provided editor currently occupying the input slot.
    pub(super) active_extension_editor: std::sync::Mutex<Option<Arc<Editor>>>,
    /// Single-line input currently occupying the input slot for an extension.
    pub(super) active_extension_input: std::sync::Mutex<Option<Arc<Input>>>,
    /// Callback used to resolve an extension dialog with a cancellation action.
    pub(super) active_extension_cancel: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// The autocomplete manager (slash + @file providers) consulted on every
    /// editor keystroke.
    pub(super) autocomplete: AutocompleteManager,
    /// The container rendered above the editor holding the live autocomplete
    /// suggestion list (cleared when there are no suggestions).
    pub(super) autocomplete_container: Arc<Container>,
    /// Maximum number of autocomplete rows rendered above the editor.
    pub(super) autocomplete_max_visible: usize,
    /// Currently highlighted autocomplete row (0-based), moved by ↑/↓.
    pub(super) autocomplete_selection: std::sync::Mutex<usize>,
    /// The completion prefix (the editor text span the popup is completing)
    /// that was current when the user LAST navigated the popup with ↑/↓.
    ///
    /// The row index alone cannot express "the user explicitly selected row
    /// 0" — ↓ then ↑ lands back on 0, indistinguishable from a fresh popup.
    /// The stamp holds the prefix the popup was completing at that moment;
    /// an Enter accepts the highlighted row only while the popup still
    /// completes that same prefix. Any change to the completed span (typing,
    /// backspace, a different token) retires the stamp, so a fresh popup for
    /// the new prefix starts un-navigated.
    pub(super) autocomplete_selection_stamped_for: std::sync::Mutex<Option<String>>,
    /// Images queued from clipboard paste and attached to the next prompt.
    pub(super) pending_images: std::sync::Mutex<Vec<rpi_ai::types::ImageContent>>,
    /// The owned theme manager — `/theme` applies presets here. The global
    /// `theme()` is read-only after OnceLock init, so per-instance state is the
    /// only way to apply a preset at runtime.
    pub(super) theme_manager: Arc<ThemeManager>,
    /// Discovered custom theme files (name → path), rebuilt on `/reload`.
    /// Empty when `--no-themes` is set or no theme dirs exist. Listing is
    /// lazy: paths only, parsed when selected.
    pub(super) theme_registry: std::sync::RwLock<crate::themes::ThemeRegistry>,
    /// The theme spec currently active (the persisted form: preset name or
    /// file path / registry name), so `/reload` can detect that the active
    /// file theme vanished and fall back.
    pub(super) active_theme_spec: std::sync::Mutex<Option<String>>,
    /// The running theme-file watcher, if the active theme is a file. Swapped
    /// whenever the active theme changes (selection or reload fallback);
    /// presets leave it `None` (nothing to poll).
    pub(super) theme_watcher: std::sync::Mutex<Option<crate::fs_watch::WatchHandle>>,
    /// The alt-screen handle, held so `set_status` can reflect run state in the
    /// terminal window title ("rpi — working" / "rpi"). `None` in unit tests
    /// that never call `set_status` with a title.
    pub(super) tui: Option<Arc<TuiAltScreen>>,
    /// The model id currently shown in the footer + used as the Ctrl+P
    /// cycle anchor. Sync-tracked (updated on every `/model`/Ctrl+P switch) so
    /// the blocking key loop can cycle without awaiting `lane.get_model()`.
    pub(super) current_model_id: std::sync::Mutex<String>,
    /// Whether inline image rendering is enabled (`/images` toggle). Stored
    /// even though image wiring is minimal this pass — the flag is consulted
    /// where images would be shown and echoed back by `/images`.
    pub(super) show_images: std::sync::Mutex<bool>,
    /// Transcript notices for prompt-cache costs and provider recovery
    /// diagnostics (upstream `showCacheMissNotices`, default `false`).
    pub(super) cache_miss_notices: std::sync::Mutex<bool>,
    /// Submitted-message history for ↑/↓ recall, most recent first (mirrors
    /// the TS editor `history` array). Bounded at [`HISTORY_LIMIT`].
    pub(super) history: std::sync::Mutex<Vec<String>>,
    /// Browse index while recalling history: -1 = not browsing, 0 = most
    /// recent, 1 = older, … Reset to -1 on every submit.
    pub(super) history_index: std::sync::Mutex<isize>,
    /// The editor text captured when entering browse mode, restored when the
    /// user navigates back past the newest entry (TS `historyDraft`).
    pub(super) history_draft: std::sync::Mutex<Option<String>>,
    /// Full cache-waste tracker (pi `detectCacheMiss`/`cache-stats.ts`):
    /// counts and prices prompt-cache misses across turns using the same
    /// noise floor, idle-gap, and model-change logic as the batch scan.
    pub(super) cache_tracker: std::sync::Mutex<rpi_harness::cache_stats::CacheMissTracker>,
    /// Set by the event drain when the `AgentEvent` broadcast reported
    /// `Lagged`, i.e. at least one event was dropped outright.
    ///
    /// `MessageUpdate` re-delivers a full snapshot, so a dropped delta is
    /// harmless — but `MessageEnd` is terminal and carries the turn's usage.
    /// Losing one freezes the footer's totals for the rest of the session, and
    /// nothing later in the stream re-sends them. The flag makes that
    /// possibility actionable at the end of the run, where an authoritative
    /// rebuild is affordable; without it every turn would have to re-read the
    /// whole branch to notice a gap that almost never happens.
    pub(super) events_lagged: std::sync::atomic::AtomicBool,
    /// The in-progress scoped-models selection while the `/scoped-models`
    /// selector is open (toggle per item, Esc saves). `None` when not editing.
    pub(super) scoped_edit: std::sync::Mutex<Option<Vec<String>>>,
    /// B5e: the live assistant-markdown transformer, built from the current
    /// `RegistrySnapshot`'s `register_markdown_transformer` handlers. `None`
    /// when no markdown transformers are registered (identity render path).
    /// Swapped on `/reload` (a fresh snapshot ⇒ a fresh closure; the old
    /// closure no-ops once its snapshot's `active` flag flips false) and
    /// re-installed on the in-flight `current_assistant` so a reloaded plugin's
    /// transform takes effect on the visible streaming message immediately.
    /// New assistant components pick up whatever closure is current at
    /// construction time via [`install_markdown_transformer`].
    pub(super) markdown_transformer: Arc<std::sync::Mutex<Option<MarkdownTransformer>>>,
    /// Live extension registry used by message/entry renderer dispatch.
    pub(super) extension_session: crate::session::ExtensionSessionCell,
    /// Releases for already-claimed presses must reach the plugin even if a
    /// draft or modal appears while the key is held (e.g. push-to-talk).
    pub(super) extension_claimed_keys: Mutex<std::collections::HashSet<KeyCode>>,
    /// Transcript search handler (Ctrl+Shift+F).
    pub(super) search: Arc<AltScreenSearch>,
    /// Search bar component shown when search is active.
    pub(super) search_bar: Arc<SearchBar>,
    /// Dock slot listing queued steering/follow-up messages while a run is
    /// active (upstream's `pendingMessagesContainer`). Empty when nothing is
    /// queued, so it renders zero rows and never affects the layout.
    pub(super) pending_container: Arc<Container>,
    /// Display text for the `app.message.dequeue` binding (e.g. `Alt+Q`),
    /// shown in the pending-messages hint row.
    pub(super) dequeue_hint: String,
    /// Last queue snapshot rendered into `pending_container`. Lets
    /// `set_pending_queue` skip redundant rebuilds + frame requests when the
    /// polled queue is unchanged.
    pub(super) pending_snapshot: std::sync::Mutex<rpi_harness::agent_harness::QueuedMessages>,
}

/// How many submitted messages are kept for ↑ recall (mirrors the TS
/// editor's 100-entry cap).
pub(super) const HISTORY_LIMIT: usize = 100;

/// Footer-status prefix used while a voice/extension draft counts down to an
/// auto-send. Doubles as the marker that lets [`TuiState::clear_auto_send`]
/// know the footer is showing *our* text before blanking it.
pub(super) const AUTO_SEND_PREFIX: &str = "Auto-send in ";

/// upstream scrolls a small chunk for each wheel notch rather than moving the
/// transcript one physical row at a time. Three lines stays precise while
/// avoiding the sluggish feel of the previous implementation.
pub(super) const MOUSE_WHEEL_SCROLL_LINES: i32 = 3;

/// Parse the compact key notation used by upstream settings (for example
/// `ctrl+g`, `shift+tab`, or `escape`) into crossterm's representation.
pub(super) fn parse_configured_key(value: &str) -> Option<rpi_tui::KeyCombo> {
    let mut modifiers = KeyModifiers::NONE;
    let mut key = None;
    for part in value.trim().to_ascii_lowercase().split('+') {
        match part {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            "alt" | "option" => modifiers |= KeyModifiers::ALT,
            "super" | "cmd" | "command" | "meta" => modifiers |= KeyModifiers::SUPER,
            part if !part.is_empty() => key = Some(part.to_string()),
            _ => {}
        }
    }
    let key = key?;
    let code = match key.as_str() {
        "esc" | "escape" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "tab" => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                return Some(rpi_tui::KeyCombo::new(
                    KeyCode::BackTab,
                    modifiers & !KeyModifiers::SHIFT,
                ));
            }
            KeyCode::Tab
        }
        "backspace" | "back" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" | "arrowup" => KeyCode::Up,
        "down" | "arrowdown" => KeyCode::Down,
        "left" | "arrowleft" => KeyCode::Left,
        "right" | "arrowright" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page-up" => KeyCode::PageUp,
        "pagedown" | "page-down" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        "f1" => KeyCode::F(1),
        "f2" => KeyCode::F(2),
        "f3" => KeyCode::F(3),
        "f4" => KeyCode::F(4),
        "f5" => KeyCode::F(5),
        "f6" => KeyCode::F(6),
        "f7" => KeyCode::F(7),
        "f8" => KeyCode::F(8),
        "f9" => KeyCode::F(9),
        "f10" => KeyCode::F(10),
        "f11" => KeyCode::F(11),
        "f12" => KeyCode::F(12),
        value if value.chars().count() == 1 => KeyCode::Char(value.chars().next().unwrap()),
        _ => return None,
    };
    Some(rpi_tui::KeyCombo::new(code, modifiers))
}

pub(crate) fn configured_keybindings() -> Arc<rpi_tui::Keybindings> {
    let mut bindings = rpi_tui::Keybindings::new();
    let settings = crate::settings::load_settings().unwrap_or_default();
    let Some(overrides) = settings.keybindings else {
        rpi_tui::set_keybindings(bindings.clone());
        return Arc::new(bindings);
    };
    let known: &[(&str, rpi_tui::KeybindingId)] = &[
        ("app.interrupt", rpi_tui::keybindings::keys::INTERRUPT),
        ("app.clear", rpi_tui::keybindings::keys::CLEAR),
        ("app.exit", rpi_tui::keybindings::keys::EXIT),
        ("app.model.select", rpi_tui::keybindings::keys::MODEL_SELECT),
        (
            "app.model.cycleForward",
            rpi_tui::keybindings::keys::MODEL_CYCLE_FORWARD,
        ),
        ("app.tools.expand", rpi_tui::keybindings::keys::TOOLS_EXPAND),
        (
            "app.thinking.toggle",
            rpi_tui::keybindings::keys::THINKING_TOGGLE,
        ),
        (
            "app.editor.external",
            rpi_tui::keybindings::keys::EXTERNAL_EDITOR,
        ),
        (
            "app.thinking.cycle",
            rpi_tui::keybindings::keys::THINKING_CYCLE,
        ),
        (
            "app.clipboard.pasteImage",
            rpi_tui::keybindings::keys::PASTE_IMAGE,
        ),
        ("app.message.dequeue", rpi_tui::keybindings::keys::DEQUEUE),
        ("tui.altScreen.bottom", rpi_tui::keybindings::keys::BOTTOM),
        ("tui.altScreen.top", rpi_tui::keybindings::keys::TOP),
        ("tui.altScreen.pageUp", rpi_tui::keybindings::keys::PAGE_UP),
        (
            "tui.altScreen.pageDown",
            rpi_tui::keybindings::keys::PAGE_DOWN,
        ),
    ];
    for (name, id) in known {
        let Some(value) = overrides.get(*name) else {
            continue;
        };
        let values: Vec<String> = match value {
            serde_json::Value::String(value) => vec![value.clone()],
            serde_json::Value::Array(values) => values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            serde_json::Value::Null => Vec::new(),
            _ => continue,
        };
        let combos: Vec<_> = values
            .iter()
            .filter_map(|value| parse_configured_key(value))
            .collect();
        if values.is_empty() || !combos.is_empty() {
            bindings.set(id, combos);
        }
    }
    rpi_tui::set_keybindings(bindings.clone());
    Arc::new(bindings)
}

pub(super) fn keybinding_matches(
    bindings: &rpi_tui::Keybindings,
    event: &crossterm::event::KeyEvent,
    id: rpi_tui::KeybindingId,
) -> bool {
    if bindings.matches(event, id) {
        return true;
    }
    // crossterm reports Shift+Tab as BackTab on some terminals and as Tab
    // plus Shift on others. Treat both forms as the same configured action.
    if event.code == KeyCode::BackTab {
        let normalized =
            crossterm::event::KeyEvent::new(KeyCode::Tab, event.modifiers | KeyModifiers::SHIFT);
        bindings.matches(&normalized, id)
    } else {
        false
    }
}

/// Display text for the `app.message.dequeue` binding (e.g. `Alt+Q`), used in
/// the pending-messages hint row. Mirrors upstream's `getAppKeyDisplay`;
/// falls back to upstream's platform default when nothing is configured.
pub(super) fn pending_dequeue_hint(bindings: &rpi_tui::Keybindings) -> String {
    let keys = bindings.get_keys(rpi_tui::keybindings::keys::DEQUEUE);
    if keys.is_empty() {
        return if cfg!(windows) { "Alt+Q" } else { "Alt+Up" }.to_string();
    }
    keys.iter()
        .map(|combo| combo.display())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether `last`/`now` fall inside the same 500ms double-press window. Native
/// pi uses one window for both Esc's double-escape action and Ctrl+C's "press
/// twice to exit", so this helper serves both.
pub(super) fn double_escape_trigger(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    last.is_some_and(|previous| {
        now.duration_since(previous) <= std::time::Duration::from_millis(500)
    })
}

/// What one Ctrl+C press should do, per upstream's `handleCtrlC`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CtrlCAction {
    /// First press: clear the editor and arm the double-press window.
    Clear,
    /// The window was already open → quit.
    Exit,
}

/// upstream's `handleCtrlC`: a press inside the 500ms window quits, otherwise
/// it clears. It never aborts a run — cancelling is `app.interrupt` (Esc)'s job.
pub(super) fn ctrl_c_action(
    last_sigint: Option<std::time::Instant>,
    now: std::time::Instant,
) -> CtrlCAction {
    if double_escape_trigger(last_sigint, now) {
        CtrlCAction::Exit
    } else {
        CtrlCAction::Clear
    }
}

/// How long to wait for queued console input before treating a bare Enter as a
/// real submit. Pasted input is already in the queue, so this only needs to
/// cover scheduler latency — small enough that a human Enter feels instant.
pub(super) const PASTE_PROBE: std::time::Duration = std::time::Duration::from_millis(4);

/// Maximum gap between a pasted character and the next pasted key. A human
/// cannot deliver two keypresses this fast, so anything tighter is paste.
pub(super) const PASTE_BURST_GAP: std::time::Duration = std::time::Duration::from_millis(20);

/// Text volume worth folding into a `[paste #n …]` marker, and worth the
/// native console drain that slurps the rest of a clipboard payload in one
/// syscall.
pub(super) const PASTE_RUN_MIN_CHARS: usize = 128;

/// How long a flushed *small* run stays available as evidence that a
/// trailing Enter belongs to it.
///
/// The console can deliver one clipboard payload in several batches, so the
/// batch holding the final newline may arrive after the text batch was already
/// handed to the editor. Small runs keep this short: a coalesced IME commit
/// (two characters in one queued batch) sets `collected` too, and its evidence
/// must be gone well before a user's next deliberate Enter (~200 ms away).
pub(super) const PASTE_CARRY_GAP: std::time::Duration = std::time::Duration::from_millis(120);

/// How long a *mature* paste — one that reached the fold threshold — stays
/// available as evidence. A large clipboard payload is delivered in many
/// batches, so the batch holding the final newline can trail the text batches
/// by hundreds of milliseconds.
pub(super) const PASTE_MATURE_CARRY_GAP: std::time::Duration =
    std::time::Duration::from_millis(500);

/// Paste evidence kept after a run was flushed to the editor.
///
/// The console can hand one clipboard payload over in several batches, each of
/// which takes the empty-queue flush path, so one paste's batches must
/// **accumulate** (`text`): corroborating only the last batch would lose the
/// earlier ones and the clipboard prefix would never match. But two *separate*
/// pastes inside the window must not be glued, so the most recent batch is also
/// kept alone (`last`): the corroborated text is each candidate in turn, and
/// whichever matches the clipboard wins.
///
/// Each variant carries its **own** lifetime: a batch that reached the fold
/// threshold on its own keeps the long window, one that only adds a few
/// characters to an old accumulated paste keeps the short one. Maturity is
/// decided per variant so an older large paste cannot promote a new short
/// input (a coalesced IME commit) to the 500 ms window.
///
/// One more fact separates IME from paste where even the queue probe cannot:
/// **shape**. A coalesced IME commit is queued behind itself, so the probe
/// marks it `collected` exactly like a paste's first batch — but it is one
/// batch, it is short, and it never contains a newline. A lone short batch is
/// therefore not trusted for the whole short carry window: it corroborates
/// only while an Enter could still be trailing its own batch at machine
/// speed (`PASTE_BURST_GAP`, far below any human key-to-key delay), and only
/// through the variant it actually shaped. Batch order inside one clipboard
/// payload is preserved but not atomic — the payload's own newline can be the
/// next console batch while the rest of the payload is still in transit, so
/// order alone cannot retire a lone short tail, and a clipboard prefix match
/// beyond machine speed is coincidence, not evidence.
pub(super) struct PasteTail {
    /// All batches flushed within the carry window, concatenated.
    pub(super) text: String,
    /// The most recent batch alone.
    pub(super) last: String,
    /// Whether the accumulated text reached the fold threshold: the
    /// accumulated variant gets the longer carry window.
    pub(super) mature: bool,
    /// Whether the most recent batch alone reached the fold threshold: the
    /// `last` variant gets its own, independently computed window.
    pub(super) last_mature: bool,
    /// Whether the accumulated text is newline-less and short as a whole —
    /// the shape of one coalesced IME commit, or of several concatenated
    /// ones. A real paste accumulating past one batch either carries an
    /// interior newline in a batch or reaches the fold threshold; IME
    /// commits never contain a newline.
    pub(super) lone_short: bool,
    /// Whether the most recent batch is newline-less and short on its own,
    /// independent of what was carried: a carried history must not re-enable
    /// a lone short `last` candidate.
    pub(super) last_single_batch: bool,
    pub(super) at: std::time::Instant,
}

/// Editor ownership and clipboard evidence for incrementally folding one paste.
/// This is deliberately separate from `PasteTail`: folding must never become
/// another reason to consume a submitting Enter.
struct WindowsFoldSession {
    editor: rpi_tui::editor::PasteSession,
    // Short first batches are displayed before reading the clipboard. Only a
    // later paste-shaped chunk can corroborate and refold that pending prefix.
    clipboard: Option<String>,
    received: String,
    // Visible, provisional suffix since the last corroborated continuation.
    // Check the whole suffix, not each scalar, for new-paste ambiguity.
    pending: String,
    at: std::time::Instant,
}

/// Windows crossterm reads console KEY_EVENTs, not bracketed-paste payloads, so
/// a clipboard paste arrives as ordinary `KeyCode::Char` events.
///
/// Collect continuous text until a short idle deadline before displaying it.
/// A burst waits 120ms to avoid showing a paste prefix before it can fold;
/// isolated characters use 15ms. Multi-character IME commits share this delay.
/// Queue probing and clipboard corroboration still classify pasted newlines
/// and join payloads that cross the display window.
#[derive(Default)]
pub(super) struct WindowsPasteRun {
    pub(super) text: String,
    pub(super) last_at: Option<std::time::Instant>,
    pub(super) burst: bool,
    /// Real input was queued behind one of this run's characters. This is
    /// newline evidence, not a physical paste boundary: coalesced IME commits
    /// can set it too. Shape and input age keep their submitting Enter safe.
    pub(super) collected: bool,
    /// The most recent batch of this run already handed to the editor, kept
    /// while a trailing newline could still belong to it.
    pub(super) tail: Option<PasteTail>,
    fold_session: Option<WindowsFoldSession>,
}

impl WindowsPasteRun {
    /// Idle collection window, matching the original Windows buffering policy.
    pub(super) fn grace(&self) -> std::time::Duration {
        std::time::Duration::from_millis(if self.burst { 120 } else { 15 })
    }

    /// Time left from the last text character, not from the last poll or mouse
    /// event. Unrelated console events must not keep pending text hidden.
    pub(super) fn remaining_delay(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        if !self.is_buffering() {
            return None;
        }
        Some(self.last_at.map_or(std::time::Duration::ZERO, |at| {
            self.grace()
                .saturating_sub(now.saturating_duration_since(at))
        }))
    }

    /// Whether anything is buffered, i.e. the current run is being treated as a
    /// paste.
    pub(super) fn is_buffering(&self) -> bool {
        !self.text.is_empty()
    }

    /// Whether the run is long enough to be worth folding into a `[paste #n]`
    /// marker instead of inserting verbatim.
    ///
    /// Size — not elapsed time — is what separates a clipboard paste from an
    /// IME commit: `我们` arrives back-to-back exactly like a two-character
    /// paste, so only a run past this threshold is treated as pasted content.
    pub(super) fn is_paste_candidate(&self) -> bool {
        self.burst && self.text.chars().count() >= PASTE_RUN_MIN_CHARS
    }

    /// Whether the per-character queue probe still has to run.
    ///
    /// An established paste is drained straight out of the console instead:
    /// probing first would stash the next event, and that stash is exactly what
    /// made the native batch read unreachable.
    pub(super) fn probe_needed(&self) -> bool {
        !self.is_paste_candidate()
    }

    /// Record queued-input evidence from the probe or native batch drain.
    /// This flag alone cannot distinguish a paste from an IME commit.
    pub(super) fn mark_collected(&mut self) {
        self.collected = true;
    }

    /// Buffer one character of a paste run.
    pub(super) fn feed(&mut self, ch: char, now: std::time::Instant) {
        if self
            .last_at
            .is_some_and(|at| now.duration_since(at) <= PASTE_BURST_GAP)
        {
            self.burst = true;
        }
        self.text.push(ch);
        self.last_at = Some(now);
    }

    #[cfg(windows)]
    pub(super) fn read_queued_text(&mut self) {
        // Drain the rest of a confirmed paste straight out of the console: the
        // round trip per record is what makes a large paste slow to appear.
        if self.is_paste_candidate() {
            if let Ok(text) = rpi_tui::windows_paste::read_text_batch() {
                if !text.is_empty() {
                    self.text.push_str(&text);
                    self.last_at = Some(std::time::Instant::now());
                    self.mark_collected();
                }
            }
        }
    }

    /// Whether an Enter arriving now could still belong to a paste: a tail
    /// variant was flushed recently, or text is being collected right now.
    ///
    /// Cheap pre-check for the caller, which would otherwise clone the tail on
    /// every Enter just to learn that there is nothing to corroborate.
    pub(super) fn has_paste_evidence(&self, now: std::time::Instant) -> bool {
        self.tail.as_ref().is_some_and(|tail| {
            Self::variant_fresh(tail, tail.mature, now)
                || Self::variant_fresh(tail, tail.last_mature, now)
        }) || (self.burst && !self.text.is_empty())
    }

    /// Whether one tail variant is still inside the carry window its own
    /// maturity earned.
    ///
    /// Both variants share the flush instant; only the window differs, and it
    /// is decided per variant: a short batch flushed on top of an old large
    /// paste must not inherit the large paste's 500 ms window.
    fn variant_fresh(tail: &PasteTail, mature: bool, now: std::time::Instant) -> bool {
        let gap = if mature {
            PASTE_MATURE_CARRY_GAP
        } else {
            PASTE_CARRY_GAP
        };
        now.duration_since(tail.at) <= gap
    }

    /// Texts a trailing newline could still belong to, longest first.
    ///
    /// Empty means nothing here was a paste, so an Enter cannot be pasted
    /// content. The candidates depend on what the run is doing:
    ///
    /// - **Still buffering** (live text): continuations — each *fresh* carried
    ///   variant joined with the live text, then the live text alone (the head
    ///   of a *new* paste). The carry **by itself is deliberately absent**: once
    ///   the user edits on top of a completed paste, the live text continues
    ///   neither carried variant, and an old prefix match must not override
    ///   those edits by swallowing the Enter that submits them.
    /// - **Idle** (nothing buffered): a trailing batch may still arrive, so
    ///   each fresh carried variant corroborates on its own — whole, then the
    ///   most recent batch alone (the head of a new paste). Each variant only
    ///   outlives the window its own length earned.
    /// - **No carry**: a live run corroborates when the console proved it is a
    ///   paste (`collected`) or its text already contains a newline. Queue
    ///   occupancy alone cannot separate a paste's first line from a coalesced
    ///   IME commit. Idle buffering can leave either live at Enter time, so
    ///   `windows_enter_in_paste` additionally rejects a lone short live
    ///   candidate after machine-speed continuation has expired. The newline
    ///   gate covers the boundary before any pasted newline has been accepted.
    ///
    /// Returning the candidates separately — rather than one glued string —
    /// is what keeps one paste's continuation apart from two pastes'
    /// boundary: the caller matches each against the clipboard and any match
    /// wins.
    pub(super) fn paste_tail_texts(&self, now: std::time::Instant) -> Vec<String> {
        let live = self.text.as_str();
        let Some(tail) = self.tail.as_ref() else {
            if self.collected || live.contains('\n') {
                return vec![live.to_string()];
            }
            return Vec::new();
        };
        if live.is_empty() {
            let mut candidates = Vec::new();
            // Each variant is gated by the shape *it* has, and a lone short
            // newline-less variant corroborates only while an Enter could
            // still be trailing its batch at machine speed. The payload's own
            // newline may trail at machine speed — the first-newline batch
            // boundary — but a human's Enter (IME confirm, then Enter: two
            // actions) never does, so beyond `PASTE_BURST_GAP` the shape is
            // an IME commit and a clipboard prefix match is coincidence. A
            // carried history must not re-enable a lone short `last`
            // candidate: `last_single_batch` is computed from the batch
            // alone, independent of the carry.
            let lone_short_expired = |shape: bool, mature: bool| {
                shape && !mature && now.duration_since(tail.at) > PASTE_BURST_GAP
            };
            if Self::variant_fresh(tail, tail.mature, now)
                && !lone_short_expired(tail.lone_short, tail.mature)
            {
                candidates.push(tail.text.clone());
            }
            if tail.last != tail.text
                && Self::variant_fresh(tail, tail.last_mature, now)
                && !lone_short_expired(tail.last_single_batch, tail.last_mature)
            {
                candidates.push(tail.last.clone());
            }
            return candidates;
        }
        let mut candidates = Vec::new();
        if Self::variant_fresh(tail, tail.mature, now) {
            candidates.push(format!("{}{live}", tail.text));
        }
        if Self::variant_fresh(tail, tail.last_mature, now) && tail.last != tail.text {
            candidates.push(format!("{}{live}", tail.last));
        }
        candidates.push(live.to_string());
        candidates
    }

    /// Hand the buffered run to the editor.
    ///
    /// Queued-input evidence retains a [`PasteTail`] for a newline delivered
    /// in a later batch. A coalesced IME commit can retain a tail too; its short
    /// shape and original input timestamp prevent swallowing a human Enter.
    /// Waiting to display the text must not renew that evidence's lifetime.
    #[cfg(test)]
    pub(super) fn flush(&mut self, editor: &Editor, now: std::time::Instant) {
        self.flush_with_clipboard(editor, now, || None);
    }

    /// Flush immediately, using clipboard evidence only to group the display
    /// of chunks already received. Never wait for or insert missing content.
    pub(super) fn flush_with_clipboard(
        &mut self,
        editor: &Editor,
        now: std::time::Instant,
        clipboard: impl FnOnce() -> Option<String>,
    ) {
        if !self.text.is_empty() {
            self.insert_paste_chunk(editor, now, clipboard);
            if self.burst && self.collected {
                // Accumulate, never replace: batches of *one* payload all take
                // this path, so dropping the earlier ones would make the
                // clipboard prefix unmatchable. The boundary between two
                // independent pastes is not decided here — the candidate list
                // keeps the last batch alone for exactly that case.
                let batch = std::mem::take(&mut self.text);
                let carried = self
                    .tail
                    .as_ref()
                    .filter(|tail| Self::variant_fresh(tail, tail.mature, now))
                    .map(|tail| tail.text.clone())
                    .unwrap_or_default();
                let text = format!("{carried}{batch}");
                let mature = text.chars().count() >= PASTE_RUN_MIN_CHARS;
                self.tail = Some(PasteTail {
                    last_mature: batch.chars().count() >= PASTE_RUN_MIN_CHARS,
                    lone_short: !text.contains('\n') && !mature,
                    last_single_batch: !batch.contains('\n')
                        && batch.chars().count() < PASTE_RUN_MIN_CHARS,
                    text,
                    last: batch,
                    mature,
                    // Evidence ages from input, not delayed presentation.
                    // Otherwise a 120ms-old IME word would look freshly pasted
                    // immediately after the idle flush and eat a typed Enter.
                    at: self.last_at.unwrap_or(now),
                });
            } else {
                self.tail = None;
            }
        }
        self.text.clear();
        self.burst = false;
        self.collected = false;
        self.last_at = None;
    }

    /// Explicit new pastes / non-text actions terminate both kinds of carry.
    pub(super) fn end_paste_session(&mut self) {
        crate::key_trace::paste_note(|| {
            format!(
                "boundary fold={} tail={}",
                self.fold_session.is_some(),
                self.tail.is_some()
            )
        });
        self.fold_session = None;
        self.tail = None;
    }

    fn insert_paste_chunk(
        &mut self,
        editor: &Editor,
        now: std::time::Instant,
        read_clipboard: impl FnOnce() -> Option<String>,
    ) {
        crate::key_trace::paste_note(|| {
            format!(
                "chunk bytes={} chars={} lines={} burst={} collected={} previous={} received_bytes={} confirmed={} age_ms={}",
                self.text.len(), self.text.chars().count(), self.text.split('\n').count(),
                self.burst, self.collected, self.fold_session.is_some(),
                self.fold_session.as_ref().map_or(0, |s| s.received.len()),
                self.fold_session.as_ref().is_some_and(|s| s.clipboard.is_some()),
                self.fold_session.as_ref().map_or(0, |s| now.duration_since(s.at).as_millis()),
            )
        });
        let previous = self.fold_session.take().filter(|session| {
            let grace = if session.clipboard.is_some() {
                PASTE_MATURE_CARRY_GAP
            } else {
                PASTE_BURST_GAP
            };
            now.duration_since(session.at) <= grace
        });
        let evidence = self.burst && self.collected;
        let paste_shaped =
            self.text.contains(['\n', '\r']) || self.text.chars().count() >= PASTE_RUN_MIN_CHARS;
        let combined_shaped = paste_shaped
            || previous.as_ref().is_some_and(|session| {
                session.received.contains(['\n', '\r'])
                    || session.received.chars().count() + self.text.chars().count()
                        >= PASTE_RUN_MIN_CHARS
            });
        let corroborate = previous
            .as_ref()
            .is_some_and(|session| session.clipboard.is_some() || combined_shaped)
            || (evidence && paste_shaped);
        // Do not read the clipboard for an ordinary coalesced IME commit. A
        // short prefix can be displayed now and corroborated by a later batch.
        let clipboard = if corroborate {
            read_clipboard().map(|text| normalize_console_paste(&text))
        } else {
            None
        };
        let batch = normalize_console_paste(&self.text);
        crate::key_trace::paste_note(|| {
            format!(
                "evidence={evidence} shaped={paste_shaped} clipboard_read={corroborate} available={} bytes={} batch_prefix={}",
                clipboard.is_some(), clipboard.as_ref().map_or(0, String::len),
                clipboard.as_ref().is_some_and(|c| c.starts_with(&batch)),
            )
        });
        if let Some(mut session) = previous {
            let received = format!("{}{}", session.received, self.text);
            let normalized = normalize_console_paste(&received);
            let carry_unfolded;
            if let Some(clipboard) = clipboard.as_ref() {
                // A scalar that equals the clipboard's first scalar is not
                // enough to decide a boundary. Keep the entire provisional
                // prefix visible until later bytes disambiguate it.
                let pending = normalize_console_paste(&format!("{}{}", session.pending, self.text));
                let ambiguous = clipboard.starts_with(&pending);
                let large = clipboard.split('\n').count() > 10 || clipboard.chars().count() > 1000;
                let unchanged = session
                    .clipboard
                    .as_ref()
                    .is_none_or(|previous| previous == clipboard);
                let prefix = clipboard.starts_with(&normalized);
                let eligible = unchanged && large && !batch.is_empty() && !ambiguous && prefix;
                let appended =
                    eligible && editor.append_paste_session(&mut session.editor, &self.text);
                crate::key_trace::paste_note(|| {
                    format!(
                        "append unchanged={unchanged} large={large} ambiguous={ambiguous} cumulative_prefix={prefix} received_bytes={} common_prefix_bytes={} eligible={eligible} accepted={appended}",
                        normalized.len(),
                        clipboard.bytes().zip(normalized.bytes()).take_while(|(a, b)| a == b).count(),
                    )
                });
                if appended {
                    if normalized.len() < clipboard.len() {
                        session.received = received;
                        session.pending.clear();
                        session.clipboard = Some(clipboard.clone());
                        session.at = now;
                        self.fold_session = Some(session);
                    }
                    return;
                }
                carry_unfolded = unchanged && large && prefix && ambiguous;
                if carry_unfolded {
                    session.clipboard = Some(clipboard.clone());
                }
            } else {
                // No clipboard I/O for short IME-shaped fragments. Keeping
                // their owned, visible span is not evidence to fold them.
                carry_unfolded = !paste_shaped && (session.clipboard.is_some() || !combined_shaped);
            }
            if carry_unfolded
                && now.duration_since(session.at) <= PASTE_BURST_GAP
                && editor.append_paste_session_unfolded(&mut session.editor, &self.text)
            {
                session.received = received;
                session.pending.push_str(&self.text);
                session.at = now;
                crate::key_trace::paste_note(|| {
                    format!(
                        "decision=provisional pending_bytes={} confirmed={}",
                        session.pending.len(),
                        session.clipboard.is_some()
                    )
                });
                if session
                    .clipboard
                    .as_ref()
                    .is_none_or(|c| normalized.len() < c.len())
                {
                    self.fold_session = Some(session);
                }
                return;
            }
        }
        let can_start = match clipboard.as_ref() {
            Some(clipboard) => {
                let large = clipboard.split('\n').count() > 10 || clipboard.chars().count() > 1000;
                large && batch.len() < clipboard.len() && clipboard.starts_with(&batch)
            }
            None => !paste_shaped,
        };
        if evidence && !batch.is_empty() && can_start {
            crate::key_trace::paste_note(|| {
                format!("decision=start confirmed={}", clipboard.is_some())
            });
            self.fold_session = Some(WindowsFoldSession {
                editor: editor.start_paste_session(&self.text),
                clipboard,
                received: self.text.clone(),
                pending: String::new(),
                at: now,
            });
            return;
        }
        crate::key_trace::paste_note(|| format!("decision=independent can_start={can_start}"));
        editor.handle_paste(&self.text);
    }
}

/// Console text and clipboard text use the same newline coordinates. Keep
/// tabs/control bytes here: the editor owns its own display normalization.
fn normalize_console_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Never turn a lone Enter into a newline. A queued next key proves this is
/// an interior pasted newline. At the end of a paste, require clipboard
/// corroboration before consuming Enter; otherwise preserve submit.
///
/// Corroboration tries the candidates from [`WindowsPasteRun::paste_tail_texts`]
/// in turn: while the run is still buffering, continuations of the carried
/// paste joined with the live text, or the live text alone (the head of a new
/// paste); while it is idle, the carried window itself. When live edits match
/// no candidate, the carried paste has already been completed and consumed,
/// so the Enter submits instead of inserting another newline.
pub(super) fn windows_enter_in_paste(
    run: &WindowsPasteRun,
    more_queued: bool,
    clipboard: Option<&str>,
    now: std::time::Instant,
) -> bool {
    if more_queued {
        return true;
    }
    let mut candidates = run.paste_tail_texts(now);
    // An IME word may still be buffered when a human presses Enter. Its
    // clipboard prefix alone must not turn that submit into a newline.
    // A fresh carried paste plus this suffix remains a valid candidate.
    if !run.text.contains(['\n', '\r'])
        && run.text.chars().count() < PASTE_RUN_MIN_CHARS
        && run
            .last_at
            .is_some_and(|at| now.saturating_duration_since(at) > PASTE_BURST_GAP)
    {
        candidates.retain(|candidate| candidate != &run.text);
    }
    if candidates.is_empty() {
        return false;
    }
    clipboard.is_some_and(|clipboard| {
        let normalized = clipboard.replace("\r\n", "\n").replace('\r', "\n");
        normalized.matches('\n').count() >= 10
            && candidates.iter().any(|pasted| {
                let prefix = format!("{pasted}\n");
                normalized.starts_with(&prefix)
            })
    })
}

/// Flush pending text before a shortcut or editor action can clear/move it.
/// Bare Enter must first be classified against the live paste run, then
/// flushed before autocomplete/submission; releases are never barriers.
pub(super) fn windows_paste_key_barrier(key: &KeyEvent) -> bool {
    if !should_dispatch_key(key.kind) {
        return false;
    }
    match key.code {
        KeyCode::Enter if key.modifiers.is_empty() => false,
        KeyCode::Char(_)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            false
        }
        _ => true,
    }
}

/// Whether the key loop should drain the console natively instead of probing
/// per character.
///
/// Reaching this needs an established paste plus no extension holding raw key
/// ownership. The caller evaluates it *before* any probe, so the drain cannot be
/// pre-empted by a stashed event.
#[cfg(any(windows, test))]
pub(super) fn should_drain_queued_text(
    run: &WindowsPasteRun,
    extension_claimed_keys_empty: bool,
) -> bool {
    run.is_paste_candidate() && extension_claimed_keys_empty
}

/// Whether a bare Enter is pasted content (insert a newline) rather than a
/// submit.
///
/// `crossterm` 0.27 only implements bracketed paste on Unix; on Windows the
/// console event source never produces `Event::Paste`, so a pasted block
/// arrives as plain key events with a bare Enter per line. Two timing signals
/// separate a pasted Enter from a typed one:
///
/// - `more_queued`: real key input is already sitting in the console input
///   queue. Callers must feed this through [`probe_paste_input`], which drops
///   the key's own Release events — Windows queues press+release together over
///   RDP, and a bare poll cannot tell that Release apart from paste content.
/// - `last_text_key_at`: the Enter lands within [`PASTE_BURST_GAP`] of the
///   preceding pasted character, which also catches a paste that *ends* with a
///   newline (nothing queued behind it, but it was not typed by hand).
///
/// NOTE: the caller gates this on `cfg!(windows)`. On Unix, bracketed paste
/// delivers real pastes as `Event::Paste`, and the timing heuristic would
/// misclassify a remote/mobile client's coalesced text+Enter burst as paste
/// ("回车变成了换行").
#[cfg(test)]
pub(super) fn enter_is_paste_burst(
    last_text_key_at: Option<std::time::Instant>,
    now: std::time::Instant,
    more_queued: bool,
) -> bool {
    if more_queued {
        return true;
    }
    last_text_key_at.is_some_and(|previous| now.duration_since(previous) < PASTE_BURST_GAP)
}

/// Decide whether more *real* input is queued behind the current key, for the
/// Windows paste-burst check.
///
/// Windows emits a `KeyEventKind::Release` for every press, usually queued
/// right behind the `Press`. A bare non-blocking poll therefore reports "more
/// input" for a lone Enter — which made a remote/RDP Enter look like a paste
/// burst. This drains and discards release events and returns the first
/// *meaningful* event it read (stashed so the caller's loop still processes it)
/// plus whether any real input was queued. `next_event` returns `None` when
/// nothing is available within the probe window.
pub(super) fn probe_paste_input(
    pending: Option<Event>,
    mut next_event: impl FnMut() -> Option<Event>,
) -> (bool, Option<Event>) {
    let mut pending = pending;
    let mut more_queued = pending.is_some();
    while !more_queued {
        match next_event() {
            Some(Event::Key(key)) if key.kind == KeyEventKind::Release => continue,
            Some(ev) => {
                pending = Some(ev);
                more_queued = true;
            }
            None => break,
        }
    }
    (more_queued, pending)
}

#[cfg(test)]
pub(super) use crate::transcript_navigation::transcript_page_size;

pub(super) fn should_dispatch_key(kind: KeyEventKind) -> bool {
    kind != KeyEventKind::Release
}

/// Last-resort quit, run on the key worker thread.
///
/// The async loop owns `run_prompt_streaming(..).await` for the whole run, so a
/// queued [`TuiMessage::Exit`] is not read while a run is in flight. When the
/// user asks to quit a second time — the first Ctrl+C only reached `Aborting` —
/// treat the run as wedged, restore the terminal, and exit.
///
/// `130` is the conventional `128 + SIGINT` status, so a wrapper script can tell
/// a forced quit from a clean one.
pub(super) fn emergency_exit(tui: &Arc<TuiAltScreen>) -> ! {
    // Best effort: leave the alternate buffer and restore cooked mode so the
    // user's shell is usable afterwards. `stop` also joins the render
    // scheduler, so no in-flight frame can repaint over the restored screen.
    tui.stop(Default::default());
    eprintln!("\n\x1b[2mrpi: aborted (forced quit).\x1b[0m");
    std::process::exit(130);
}

/// upstream's `handleCtrlZ` (`app.suspend`): hand the terminal back to the
/// shell and stop the process. `raise(SIGTSTP)` returns once the user resumes
/// us with `fg`/`bg`, at which point we re-enter the alternate screen.
#[cfg(unix)]
pub(super) fn handle_suspend(tui: &Arc<TuiAltScreen>) {
    tui.suspend();
    // A `Result` here would mean the signal could not be raised; there is no
    // useful recovery, so resume either way and let the user retry.
    let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP);
    tui.resume();
}

/// upstream binds no suspend key on Windows (`app.suspend`'s `defaultKeys` is
/// empty there), so [`keys::SUSPEND`] never matches and this is unreachable.
#[cfg(not(unix))]
pub(super) fn handle_suspend(_tui: &Arc<TuiAltScreen>) {}

/// Compact token count for the cache-miss notice: 1.2M / 34.5K / 900.
pub(super) fn format_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Record a submitted message for ↑ recall (mirrors TS `addToHistory`):
/// trims, skips empty + consecutive duplicates, caps at [`HISTORY_LIMIT`], and
/// resets the browse state so a fresh prompt never resumes mid-history.
///
/// The browse reset is unconditional: resubmitting a recalled entry is a
/// duplicate and adds nothing to history, but the session still ended — a
/// stale `history_index` would leave the next ↓ restoring a draft that had
/// already been submitted.
pub(super) fn push_history(state: &Arc<TuiState>, text: &str) {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    {
        let mut history = state.history.lock().unwrap();
        if history.first() != Some(&trimmed) {
            history.insert(0, trimmed);
            history.truncate(HISTORY_LIMIT);
        }
    }
    *state.history_index.lock().unwrap() = -1;
    *state.history_draft.lock().unwrap() = None;
}

/// Submit a draft as a user prompt through the *same* path the Enter key takes:
/// render the user bubble, record history, then hand the text to the main loop
/// as [`TuiMessage::UserInput`]. Using the editor path (rather than a direct
/// `SendUserMessage`) is what makes a voice transcription show up as a user
/// message — the harness emits no user `message_start` for a directly-sent
/// prompt.
pub(super) fn submit_draft(
    state: &Arc<TuiState>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    text: &str,
) {
    if text.trim().is_empty() || !state.try_start_working() {
        return;
    }
    add_user_message(&state.chat_container, text);
    if let Some(tui) = &state.tui {
        if let Some(scroll) = tui.get_primary_scroll_view() {
            scroll.scroll_to_end();
        }
        tui.request_render(false);
    }
    push_history(state, text);
    let _ = tx.send(TuiMessage::UserInput(
        text.to_string(),
        state.take_pending_images(),
    ));
}

/// Advance a pending auto-send countdown. Returns `true` when the caller needs
/// to repaint.
///
/// The draft is abandoned — leaving the user's text in place and clearing the
/// footer countdown — when the user edits it (the editor no longer matches the
/// injected text), when a run has started, or when the editor was cleared.
/// Otherwise the remaining time is written to the footer; once it reaches zero
/// the draft is submitted.
pub(super) fn poll_auto_send(
    state: &Arc<TuiState>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
) -> bool {
    // Clone out of the lock before touching the editor/footer so no other
    // thread can block behind this tick.
    let pending = state.auto_send.lock().unwrap().clone();
    let step = auto_send_step(
        pending.as_ref(),
        &state.editor.get_text(),
        *state.status.lock().unwrap() == RunStatus::Idle,
        std::time::Instant::now(),
    );

    match step {
        AutoSendStep::Idle => false,
        AutoSendStep::Cancel => {
            state.clear_auto_send();
            true
        }
        AutoSendStep::Submit => {
            let text = state.editor.get_text();
            state.clear_auto_send();
            state.clear_editor_programmatically();
            submit_draft(state, tx, &text);
            true
        }
        AutoSendStep::Wait(remaining) => {
            let secs = remaining.as_millis() as f64 / 1000.0;
            let label = format!("{AUTO_SEND_PREFIX}{secs:.1}s · any key to edit");
            if state.footer.get_status() != label {
                state.footer.set_status(&label);
                true
            } else {
                false
            }
        }
    }
}

/// Whether an ↑/↓ press belongs to prompt history rather than to the caret.
///
/// Inside a multi-line draft the editor owns the arrow whenever the caret can
/// still travel in that direction: ↑ moves between logical lines until it
/// reaches the first line, ↓ until it reaches the last. Only the press that
/// cannot move the caret any further switches to history, which is what makes
/// a multi-line prompt — typed or recalled — editable line by line.
///
/// A single-line draft keeps the pre-existing contract: the arrows never
/// replace it. That behaviour is deliberate — swapping a typed draft for a
/// history entry was reported as "text disappeared" — and a single-line prompt
/// has no second row to move to, so there is no "again" press to reinterpret.
/// An active browse session is the exception: after recalling a single-line
/// entry the editor is exactly that shape, and ↑/↓ must keep walking history
/// until ↓ past the newest entry ends the session.
///
/// `direction` is -1 (↑, older) or 1 (↓, newer), matching [`navigate_history`].
pub(super) fn arrow_belongs_to_history(
    state: &Arc<TuiState>,
    editor: &Arc<Editor>,
    direction: i32,
) -> bool {
    if state.history.lock().unwrap().is_empty() {
        return false;
    }
    // A fresh prompt has no caret row to move to, so the first arrow press
    // recalls immediately (upstream's `historyPrevious` on an empty editor).
    if editor.get_text().is_empty() {
        return true;
    }
    if editor.line_count() < 2 {
        return *state.history_index.lock().unwrap() != -1;
    }
    let (row, _col) = editor.cursor_position();
    if direction < 0 {
        row == 0
    } else {
        row + 1 >= editor.line_count()
    }
}

/// Navigate message history. `direction` is -1 (↑, older) or 1 (↓, newer).
/// Mirrors TS `navigateHistory`: the first entry into browse mode stashes the
/// current editor text as the draft; navigating back past the newest entry
/// restores that draft.
pub(super) fn navigate_history(state: &Arc<TuiState>, editor: &Arc<Editor>, direction: i32) {
    let history = state.history.lock().unwrap();
    if history.is_empty() {
        return;
    }
    let mut index = state.history_index.lock().unwrap();
    let new_index = *index - direction as isize;
    if new_index < -1 || new_index >= history.len() as isize {
        return;
    }
    if *index == -1 && new_index >= 0 {
        // Entering browse mode: stash the current input.
        *state.history_draft.lock().unwrap() = Some(editor.get_text());
    }
    *index = new_index;
    if new_index == -1 {
        // Exited browse mode: restore the draft (or clear if there was none).
        let draft = state.history_draft.lock().unwrap().take();
        match draft {
            Some(d) => set_editor_text_caret_at_end(editor, &d),
            None => editor.set_text(""),
        }
    } else {
        let text = history[new_index as usize].clone();
        // A recalled prompt is edited from its tail: `set_text` parks the
        // caret at (0, 0), which for a multi-line entry left ↑ as the only
        // working arrow — ↓ looked like it skipped the entry entirely.
        set_editor_text_caret_at_end(editor, &text);
    }
}

impl TuiState {
    /// Current active theme spec (the persisted form: preset name or file
    /// path / registry name).
    pub(super) fn active_theme_spec(&self) -> Option<String> {
        self.active_theme_spec
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Record the active theme spec (selection, reload fallback).
    pub(super) fn set_active_theme_spec(&self, spec: Option<String>) {
        if let Ok(mut guard) = self.active_theme_spec.lock() {
            *guard = spec;
        }
    }

    pub(super) fn cancel_js_preparation(&self) -> bool {
        let cancellation = self.js_preparation_cancel.lock().unwrap().take();
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            true
        } else {
            false
        }
    }

    /// Whether a user-initiated `!command` shell run is currently active.
    pub(super) fn user_bash_running(&self) -> bool {
        self.user_bash_cancel.lock().unwrap().is_some()
    }

    /// Register a cancellation token for a new user bash run, cancelling any
    /// previous one (mirrors `begin_js_preparation`).
    pub(super) fn begin_user_bash(&self) -> CancellationToken {
        let cancellation = CancellationToken::new();
        if let Some(previous) = self
            .user_bash_cancel
            .lock()
            .unwrap()
            .replace(cancellation.clone())
        {
            previous.cancel();
        }
        self.history_page.invalidate();
        if self.history_page.is_bound() && self.history_page.is_latest() {
            self.history_page.mount_active(&self.chat_container);
        }
        cancellation
    }

    /// Start a provider OAuth flow: allocate its id and publish the id and its
    /// cancellation token in one step under the token lock, so an Esc that ends
    /// the flow always finds the token to cancel. A previous flow's token is
    /// cancelled, so a call from an older flow cannot outlive it.
    pub(super) fn begin_provider_auth(&self) -> (u64, CancellationToken) {
        let mut slot = self.provider_auth_cancel.lock().unwrap();
        let flow_id = self
            .provider_auth_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .wrapping_add(1);
        let cancellation = CancellationToken::new();
        if let Some(previous) = slot.replace(cancellation.clone()) {
            previous.cancel();
        }
        self.provider_auth_flow_id
            .store(flow_id, std::sync::atomic::Ordering::Release);
        (flow_id, cancellation)
    }

    /// The token for flow `flow_id`'s next provider call. If that flow is no
    /// longer active it is already cancelled, so a late submit cannot run
    /// unobserved or borrow a newer flow's token.
    pub(super) fn provider_auth_token(&self, flow_id: u64) -> CancellationToken {
        let slot = self.provider_auth_cancel.lock().unwrap();
        match slot.as_ref() {
            Some(cancellation)
                if self
                    .provider_auth_flow_id
                    .load(std::sync::atomic::Ordering::Acquire)
                    == flow_id =>
            {
                cancellation.clone()
            }
            _ => {
                let cancellation = CancellationToken::new();
                cancellation.cancel();
                cancellation
            }
        }
    }

    /// End the active provider OAuth flow, if any, and cancel its call. Returns
    /// whether a flow was active, i.e. this end won it from a completing result.
    pub(super) fn end_provider_auth(&self) -> bool {
        let mut slot = self.provider_auth_cancel.lock().unwrap();
        let ended = self
            .provider_auth_flow_id
            .swap(0, std::sync::atomic::Ordering::AcqRel)
            != 0;
        if let Some(cancellation) = slot.take() {
            cancellation.cancel();
        }
        ended
    }

    /// End flow `flow_id` only if it is still the active one, for a result
    /// about to write the store. Exactly one of this and an Esc's
    /// [`Self::end_provider_auth`] wins; a losing claim changes nothing.
    pub(super) fn claim_provider_auth(&self, flow_id: u64) -> bool {
        let mut slot = self.provider_auth_cancel.lock().unwrap();
        let claimed = flow_id != 0
            && self
                .provider_auth_flow_id
                .compare_exchange(
                    flow_id,
                    0,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok();
        if claimed {
            // The call already returned; this only clears the slot.
            if let Some(cancellation) = slot.take() {
                cancellation.cancel();
            }
        }
        claimed
    }

    /// Clear the user-bash slot. Called when the command finishes so the
    /// running guard and the Esc router both see idle state.
    pub(super) fn finish_user_bash(&self) {
        self.user_bash_cancel.lock().unwrap().take();
    }

    /// Cancel a running user bash command. Returns `true` when one was active
    /// (mirrors `cancel_js_preparation`). The spawned task clears the slot
    /// itself once `execute_shell_with_capture` returns.
    pub(super) fn cancel_user_bash(&self) -> bool {
        let cancellation = self.user_bash_cancel.lock().unwrap().as_ref().cloned();
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            true
        } else {
            false
        }
    }

    pub(super) fn set_status(&self, status: RunStatus) {
        *self.status.lock().unwrap() = status;
        if status == RunStatus::Working {
            self.history_page.invalidate();
            if self.history_page.is_bound() && self.history_page.is_latest() {
                self.history_page.mount_active(&self.chat_container);
            }
        }
        self.apply_status(status);
    }

    // =======================================================================
    // Footer usage accounting across the live/reconcile race
    // =======================================================================

    /// The dock container listing queued messages (upstream's
    /// `pendingMessagesContainer`).
    pub(super) fn pending_container(&self) -> &Arc<Container> {
        &self.pending_container
    }

    /// Render the current queue into the pending dock slot. Mirrors native
    /// pi's `updatePendingMessagesDisplay`: `Steering: <text>` /
    /// `Follow-up: <text>` rows (dim) plus a `↳ <key> to edit all queued
    /// messages` hint. A no-op when the snapshot is unchanged, so the poll
    /// caller can invoke it freely without forcing frames.
    pub(super) fn set_pending_queue(&self, messages: rpi_harness::agent_harness::QueuedMessages) {
        {
            let mut current = self.pending_snapshot.lock().unwrap();
            if *current == messages {
                return;
            }
            *current = messages.clone();
        }
        let container = &self.pending_container;
        container.clear();
        if messages.is_empty() {
            return;
        }
        let colors = current_theme().colors;
        container.add_child(Arc::new(Spacer::new(1)));
        for message in &messages.steering {
            container.add_child(Arc::new(Text::new(
                colors.muted.fg(&format!("Steering: {}", message.text)),
                1,
                0,
            )));
        }
        for message in &messages.follow_up {
            container.add_child(Arc::new(Text::new(
                colors.muted.fg(&format!("Follow-up: {}", message.text)),
                1,
                0,
            )));
        }
        container.add_child(Arc::new(Text::new(
            colors.muted.fg(&format!(
                "↳ {} to edit all queued messages",
                self.dequeue_hint
            )),
            1,
            0,
        )));
    }

    /// Atomically reserve the single interactive run slot. The editor callback
    /// runs on a different thread from the async prompt loop, so checking and
    /// setting in separate steps would allow rapid Enter presses to queue more
    /// than one operation.
    pub(super) fn try_start_working(&self) -> bool {
        let mut status = self.status.lock().unwrap();
        if *status != RunStatus::Idle {
            return false;
        }
        *status = RunStatus::Working;
        drop(status);
        self.history_page.invalidate();
        if self.history_page.is_bound() && self.history_page.is_latest() {
            self.history_page.mount_active(&self.chat_container);
        }
        self.apply_status(RunStatus::Working);
        true
    }

    pub(super) fn apply_status(&self, status: RunStatus) {
        match status {
            RunStatus::Working => {
                // The status dock already shows the active loader. Keep the
                // footer focused on the model and shortcuts instead of
                // repeating a second `Working…` at the bottom of the TUI.
                self.footer.set_status("");
                // Reflect the in-flight turn in the terminal window/tab title
                // (OSC 2). No-op when `tui` is absent (unit tests).
                if let Some(tui) = &self.tui {
                    tui.set_title(&crate::brand::window_title("⟳"));
                }
                self.status_container.clear();
                // The working indicator renders inside the editor's top border.
                // `show_terminal_progress` disables it entirely.
                if self.show_terminal_progress {
                    self.editor.set_working(Some(WorkingState {
                        started_at: std::time::Instant::now(),
                        message: "Working…".to_string(),
                    }));
                } else {
                    self.editor.set_working(None);
                }
            }
            RunStatus::Aborting => {
                self.footer.set_status("Aborting…");
                // Do not leave a frozen "Working" spinner on screen after the
                // render tick intentionally stops advancing in this state.
                self.loader.stop();
                self.status_container.clear();
                self.editor.set_working(None);
            }
            RunStatus::Idle => {
                self.footer.set_status("");
                if let Some(tui) = &self.tui {
                    tui.set_title(&crate::brand::window_title(""));
                }
                self.loader.stop();
                self.status_container.clear();
                self.editor.set_working(None);
            }
        }
    }

    /// Push the extension status registry into the footer when it changed.
    /// Returns `true` when the footer was rewritten (the caller then requests a
    /// repaint — an idle session would otherwise never redraw).
    pub(super) fn sync_extension_status(&self) -> bool {
        let revision = self.ext_status.revision();
        if self
            .ext_status_revision
            .load(std::sync::atomic::Ordering::SeqCst)
            == revision
        {
            return false;
        }
        self.ext_status_revision
            .store(revision, std::sync::atomic::Ordering::SeqCst);
        let hidden = if pet::active(&self.ext_status) {
            vec![pet::PET_KEY, "voice"]
        } else {
            vec![pet::PET_KEY]
        };
        self.footer
            .set_extension_status(&self.ext_status.text_except(&hidden));
        true
    }

    /// Drain the extension editor-text queue (`SetEditorText`) into the prompt
    /// editor. Returns `true` when at least one edit was applied (the caller
    /// then repaints).
    ///
    /// An edit with an `autoSendMs` arms [`AutoSendPending`] so an *untouched*
    /// draft is submitted without a keystroke; the user can still steal it back
    /// by typing (see the key loop) or by any edit that diverges from the
    /// injected text (see [`poll_auto_send`]). The injected text is the draft,
    /// so submitting it goes through the normal prompt path and renders a user
    /// bubble — unlike `SendUserMessage`, which emits no user `message_start`.
    pub(super) fn drain_editor_text(&self) -> bool {
        let mut applied = false;
        while let Some(edit) = self.editor_text.take_pending() {
            let current = self.editor.get_text();
            let text = match edit.mode {
                rpi_extensions::EditorTextMode::Replace => edit.text,
                rpi_extensions::EditorTextMode::Append => {
                    if current.is_empty() {
                        edit.text
                    } else {
                        // A blank separator keeps two dictations readable; an
                        // existing trailing space is not doubled.
                        let mut joined = current.trim_end().to_string();
                        joined.push(' ');
                        joined.push_str(&edit.text);
                        joined
                    }
                }
            };
            set_editor_text_caret_at_end(&self.editor, &text);
            // The `EditorChange` this produces is ours, not the user's.
            self.programmatic_editor_write
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let pending = edit
                .auto_send_ms
                .filter(|ms| *ms > 0)
                .map(|ms| AutoSendPending {
                    deadline: std::time::Instant::now() + std::time::Duration::from_millis(ms),
                    text: text.clone(),
                });
            *self.auto_send.lock().unwrap() = pending;
            applied = true;
        }
        applied
    }

    /// Clear the editor as **host housekeeping**, not as a user edit.
    ///
    /// The clear that follows every submit is the important one: it is the host
    /// tidying up, and reporting it as `source:"user"` told a hands-free voice
    /// extension "the human just took the keyboard" — so `/voice auto` switched
    /// itself off the instant it was enabled (the command's own editor clear
    /// arrives on the next tick, ~80ms after the mode turned on) and the
    /// microphone never stayed open. A real user clear (Esc, select-all +
    /// delete) goes through the editor itself and is still `user`.
    pub(super) fn clear_editor_programmatically(&self) {
        self.programmatic_editor_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.editor.clear();
    }

    /// Drop a pending auto-send, clearing the footer countdown when it still
    /// shows ours (so an unrelated run status is never clobbered).
    pub(super) fn clear_auto_send(&self) {
        if self.auto_send.lock().unwrap().take().is_some()
            && self.footer.get_status().starts_with(AUTO_SEND_PREFIX)
        {
            self.footer.set_status("");
        }
    }

    /// Cancel a pending auto-send because the user pressed a key. Returns
    /// `true` when a countdown was running. The key is *still* dispatched
    /// normally, so it lands in the draft as the user's first correction.
    pub(super) fn cancel_auto_send_on_key(&self) -> bool {
        let had = self.auto_send.lock().unwrap().is_some();
        if had {
            self.clear_auto_send();
        }
        had
    }

    /// Notify extensions when the prompt draft changed since the last tick.
    ///
    /// This is how a voice extension learns the user is composing again, so it
    /// can stop reading the previous answer aloud (barge-in) — the draft text
    /// itself is not shipped, only `chars`/`empty`/`source`.
    ///
    /// `source` separates a human edit from one the TUI made on a plugin's
    /// behalf (`SetEditorText`, or clearing the draft after an auto-send). A
    /// hands-free extension needs that distinction: its own injected
    /// transcription must not be mistaken for the user taking over the
    /// keyboard, which would make it stop listening on every single turn.
    ///
    /// Polled from the render tick rather than pushed from [`Editor::on_change`]:
    /// that callback slot already has an owner (the bash-mode border colour) and
    /// runs while the editor's callback lock is held, where re-entering
    /// extension code is needlessly risky.
    pub(super) fn sync_editor_change(&self) {
        use rpi_plugin_sdk::EventTag;

        let Some(payload) = self.take_editor_change() else {
            return;
        };
        // Cheap pre-check so the common (no subscriber) case never touches a
        // plugin: session lock + snapshot read only.
        let Ok(session) = self.extension_session.lock() else {
            return;
        };
        let Some(snapshot) = session.snapshot_arc() else {
            return;
        };
        if snapshot.handlers_for(EventTag::EditorChange).is_empty() {
            return;
        }
        rpi_extensions::dispatch_data_event(
            &snapshot,
            EventTag::EditorChange,
            &payload.to_string(),
        );
    }

    /// Build the `EditorChange` payload when the draft changed since the last
    /// tick, or `None` when it did not. Split out of
    /// [`Self::sync_editor_change`] so the attribution rule is unit-testable
    /// without a live extension registry.
    ///
    /// The attribution is consumed here — even when the caller then finds no
    /// subscriber — so a stale "programmatic" mark can never mislabel a later
    /// real keystroke.
    pub(super) fn take_editor_change(&self) -> Option<serde_json::Value> {
        let text = self.editor.get_text();
        {
            let mut last = self.last_editor_text.lock().unwrap();
            if *last == text {
                return None;
            }
            *last = text.clone();
        }
        let programmatic = self
            .programmatic_editor_write
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        Some(serde_json::json!({
            "chars": text.chars().count(),
            "empty": text.is_empty(),
            "source": if programmatic {
                // The TUI wrote it (an injected draft / the auto-send clear).
                "extension"
            } else {
                "user"
            },
        }))
    }

    /// The bash panel has its own `Running...` spinner. Keep the global
    /// `Working...` loader out of the status slot while any bash tool is active
    /// so the same operation is not presented as two simultaneous loaders.
    pub(super) fn sync_working_loader_with_bash(&self) {
        if *self.status.lock().unwrap() != RunStatus::Working {
            return;
        }

        // The working indicator now lives inside the editor's top border and
        // stays visible for the whole Working state, so no status-slot swap is
        // needed when bash panels (which render their own spinner in the
        // transcript) appear or disappear.
        self.status_container.clear();
    }

    pub(super) fn show_retry(&self, attempt: u32, max_retries: u32, delay_ms: u64) {
        *self.status.lock().unwrap() = RunStatus::Working;
        self.footer.set_status("");
        if let Some(tui) = &self.tui {
            tui.set_title(&crate::brand::window_title("↻"));
        }
        self.loader.stop();
        self.status_container.clear();
        self.status_container
            .add_child(Arc::new(StatusIndicator::retry(
                attempt,
                max_retries,
                std::time::Duration::from_millis(delay_ms),
            )));
    }

    /// Whether a selector overlay is currently open (routes keys to it first).
    pub(super) fn selector_open(&self) -> bool {
        self.active_selector.lock().unwrap().is_some()
    }

    pub(super) fn extension_editor_open(&self) -> bool {
        self.active_extension_editor.lock().unwrap().is_some()
    }

    pub(super) fn extension_input_open(&self) -> bool {
        self.active_extension_input.lock().unwrap().is_some()
    }

    pub(super) fn extension_dialog_open(&self) -> bool {
        self.extension_editor_open() || self.extension_input_open()
    }

    /// Drop the state that belongs to the session being left behind.
    ///
    /// The transcript container is cleared by the caller (it also rebuilds the
    /// welcome header). This method covers the state that lives outside that
    /// container: the footer's accumulated usage/context badge, the `/copy`
    /// cache, and the tool/bash component maps keyed by `tool_call_id`.
    pub(super) fn reset_session_state(&self) {
        self.history_page.reset();
        self.footer.reset_session_counters();
        *self.last_assistant_text.lock().unwrap() = String::new();
        self.current_assistant.lock().unwrap().take();
        self.tool_components.lock().unwrap().clear();
        self.bash_components.lock().unwrap().clear();
        self.pending_bash_messages.lock().unwrap().clear();
    }

    pub(super) fn set_hide_thinking(&self, hide: bool) {
        *self.hide_thinking.lock().unwrap() = hide;
        if let Some(comp) = self.current_assistant.lock().unwrap().as_ref() {
            comp.set_hide_thinking(hide);
        }
    }

    pub(super) fn hide_thinking(&self) -> bool {
        *self.hide_thinking.lock().unwrap()
    }

    pub(super) fn toggle_thinking(&self) -> bool {
        let next = !self.hide_thinking();
        self.set_hide_thinking(next);
        next
    }

    pub(super) fn toggle_tool_outputs(&self) -> bool {
        let next = !*self.tool_outputs_expanded.lock().unwrap();
        self.transcript_view().set_expanded(next);
        self.apply_history_tool_expansion(&self.chat_container);
        next
    }

    pub(super) fn apply_history_tool_expansion(&self, root: &Container) {
        let expanded = *self.tool_outputs_expanded.lock().unwrap();
        let mut pending = root.get_children();
        while let Some(child) = pending.pop() {
            if let Some(container) = child.as_any().downcast_ref::<Container>() {
                pending.extend(container.get_children());
            }
            if let Some(tool) = child.as_any().downcast_ref::<ToolExecutionComponent>() {
                tool.set_expanded(expanded);
            }
            if let Some(bash) = child.as_any().downcast_ref::<BashExecutionComponent>() {
                bash.set_expanded(expanded);
            }
        }
    }

    /// The model id currently tracked as active (footer + Ctrl+P anchor).
    pub(super) fn current_model_id(&self) -> String {
        self.current_model_id.lock().unwrap().clone()
    }

    /// Update the tracked model id + footer label after a switch (live or
    /// cycle). Called from the `/model` on_select and the Ctrl+P handler.
    pub(super) fn set_current_model(&self, model: &rpi_ai::Model) {
        *self.current_model_id.lock().unwrap() = model.id.clone();
        self.footer.set_model(&format!(
            "({}) {}",
            model.provider,
            short_model_name(&model.id)
        ));
        // A model switch changes the context window, so the `%/{window}` badge
        // must rescale against the same last-reported token count.
        self.footer.set_context_window(model.context_window as i64);
    }

    /// B5e: read a clone of the current assistant-markdown transformer (if any).
    /// New assistant components call this at construction so they render with
    /// whatever plugin `register_markdown_transformer` handlers are live.
    pub(super) fn markdown_transformer(&self) -> Option<MarkdownTransformer> {
        self.markdown_transformer.lock().unwrap().clone()
    }

    /// Streamed messages belong in the live slot, even while it is detached.
    pub(super) fn transcript_container(&self) -> Arc<Container> {
        if self.history_page.is_bound() {
            self.history_page.active.clone()
        } else {
            self.chat_container.clone()
        }
    }

    /// Keep command/background notices visible on the page being viewed.
    /// On latest history they join streamed output, so later replies follow them.
    /// Older pages have no live slot; their notices retire with the visible page.
    pub(super) fn notice_container(&self) -> Arc<Container> {
        self.history_page.notice_container(&self.chat_container)
    }

    /// Build a [`TranscriptView`] over the shared component maps. Cheap (clones
    /// a handful of `Arc`s) and lets the event drain render the transcript
    /// through the same code path as the remote client.
    ///
    /// [`TranscriptView`]: crate::transcript_view::TranscriptView
    pub(super) fn transcript_view(&self) -> crate::transcript_view::TranscriptView {
        let managed = self.history_page.is_bound();
        crate::transcript_view::TranscriptView::with_maps(
            self.transcript_container(),
            self.current_assistant.clone(),
            self.tool_components.clone(),
            self.bash_components.clone(),
            self.hide_thinking.clone(),
            self.tool_outputs_expanded.clone(),
            self.markdown_transformer.clone(),
        )
        .with_panels(if managed {
            self.history_page.panels.clone()
        } else {
            self.chat_container.clone()
        })
        .with_show_images(self.images_visible())
    }

    /// B5e: swap the live transformer. Used at startup (install the first
    /// closure built from the initial `RegistrySnapshot`) and on `/reload`
    /// (rebuild from the fresh snapshot). On a reload the reloaded plugin's
    /// transform should take effect on the VISIBLE streaming message too, so
    /// this re-installs on the in-flight `current_assistant` component — its
    /// `set_markdown_transformer` rebuilds the last blocks immediately. A
    /// `None` clears the transform (identity), e.g. a reload that unregisters
    /// every markdown transformer.
    pub(super) fn set_markdown_transformer_with_reinstall(
        &self,
        transformer: Option<MarkdownTransformer>,
    ) {
        *self.markdown_transformer.lock().unwrap() = transformer.clone();
        if let Some(comp) = self.current_assistant.lock().unwrap().as_ref() {
            comp.set_markdown_transformer(transformer);
        }
    }

    pub(super) fn queue_image(&self, image: rpi_ai::types::ImageContent) {
        self.pending_images.lock().unwrap().push(image);
    }

    pub(super) fn take_pending_images(&self) -> Vec<rpi_ai::types::ImageContent> {
        std::mem::take(&mut *self.pending_images.lock().unwrap())
    }

    pub(super) fn restore_pending_images(&self, mut images: Vec<rpi_ai::types::ImageContent>) {
        let mut pending = self.pending_images.lock().unwrap();
        images.append(&mut *pending);
        *pending = images;
    }

    pub(super) fn set_show_images(&self, show: bool) {
        *self.show_images.lock().unwrap() = show;
        let mut pending = self.chat_container.get_children();
        // The live slot can be detached while an older page is visible.
        pending.extend(self.history_page.active.get_children());
        while let Some(child) = pending.pop() {
            if let Some(container) = child.as_any().downcast_ref::<Container>() {
                pending.extend(container.get_children());
            }
            if let Some(image) = child.as_any().downcast_ref::<Image>() {
                image.set_inline_visible(show);
            }
            if let Some(assistant) = child.as_any().downcast_ref::<AssistantMessageComponent>() {
                assistant.set_show_images(show);
            }
        }
    }

    pub(super) fn images_visible(&self) -> bool {
        *self.show_images.lock().unwrap()
    }
}
