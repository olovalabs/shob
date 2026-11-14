//! Keybinding system for TUI.
//!
//! Provides configurable keyboard shortcuts for TUI actions.
//! This is a 1:1 port of the TypeScript keybindings.ts

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyModifiers};

/// A keybinding identifier.
pub type KeybindingId = &'static str;

/// A key combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyCombo {
    /// Create a new key combination.
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Create from just a key code (no modifiers).
    pub fn from_code(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Add Ctrl modifier.
    pub fn ctrl(self) -> Self {
        Self {
            modifiers: self.modifiers | KeyModifiers::CONTROL,
            ..self
        }
    }

    /// Add Shift modifier.
    pub fn shift(self) -> Self {
        Self {
            modifiers: self.modifiers | KeyModifiers::SHIFT,
            ..self
        }
    }

    /// Add Alt modifier.
    pub fn alt(self) -> Self {
        Self {
            modifiers: self.modifiers | KeyModifiers::ALT,
            ..self
        }
    }

    /// Add Super modifier.
    pub fn super_key(self) -> Self {
        Self {
            modifiers: self.modifiers | KeyModifiers::SUPER,
            ..self
        }
    }

    /// Human-readable key text for hints (mirrors upstream's `formatKeyText`,
    /// e.g. `Alt+Q`, `Ctrl+Shift+F`, `Enter`). Modifiers are ordered
    /// Ctrl, Alt, Shift, Super.
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl".into());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("Alt".into());
        }
        // `BackTab` already carries the shift; don't render it twice.
        if self.modifiers.contains(KeyModifiers::SHIFT) && self.code != KeyCode::BackTab {
            parts.push("Shift".into());
        }
        if self.modifiers.contains(KeyModifiers::SUPER) {
            parts.push("Super".into());
        }
        let key = match self.code {
            KeyCode::Char(c) => c.to_uppercase().to_string(),
            KeyCode::Enter => "Enter".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Insert => "Insert".into(),
            KeyCode::Up => "Up".into(),
            KeyCode::Down => "Down".into(),
            KeyCode::Left => "Left".into(),
            KeyCode::Right => "Right".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PageUp".into(),
            KeyCode::PageDown => "PageDown".into(),
            other => format!("{other:?}"),
        };
        parts.push(key);
        parts.join("+")
    }
}

/// Keybinding definition.
#[derive(Debug, Clone)]
pub struct KeybindingDefinition {
    pub default_keys: Vec<KeyCombo>,
    pub description: Option<&'static str>,
}

/// Keybinding conflict.
#[derive(Debug, Clone)]
pub struct KeybindingConflict {
    pub key: KeyCombo,
    pub keybindings: Vec<KeybindingId>,
}

/// Keybindings manager.
#[derive(Clone)]
pub struct Keybindings {
    definitions: HashMap<KeybindingId, KeybindingDefinition>,
    user_bindings: HashMap<KeybindingId, Vec<KeyCombo>>,
    keys_by_id: HashMap<KeybindingId, Vec<KeyCombo>>,
    conflicts: Vec<KeybindingConflict>,
}

impl Keybindings {
    /// Create a new keybindings manager with default bindings.
    pub fn new() -> Self {
        let definitions = Self::default_definitions();
        let keys_by_id = definitions
            .iter()
            .map(|(id, def)| (*id, def.default_keys.clone()))
            .collect();
        Self {
            definitions,
            user_bindings: HashMap::new(),
            keys_by_id,
            conflicts: Vec::new(),
        }
    }

    /// Get default keybinding definitions.
    fn default_definitions() -> HashMap<KeybindingId, KeybindingDefinition> {
        use KeyCode::*;
        use KeyModifiers as M;

        let mut map = HashMap::new();

        // Application-level actions. These IDs and their default keys match
        // upstream, so a copied settings.json overrides them the same way.
        map.insert(
            "app.interrupt",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Esc, M::NONE)],
                description: Some("Cancel or abort"),
            },
        );
        map.insert(
            "app.clear",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('c'), M::CONTROL)],
                description: Some("Clear editor"),
            },
        );
        map.insert(
            "app.exit",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('d'), M::CONTROL)],
                description: Some("Exit when editor is empty"),
            },
        );
        map.insert(
            "app.model.select",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('l'), M::CONTROL)],
                description: Some("Open model selector"),
            },
        );
        map.insert(
            "app.model.cycleForward",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('p'), M::CONTROL)],
                description: Some("Cycle to next model"),
            },
        );
        map.insert(
            "app.model.cycleBackward",
            KeybindingDefinition {
                // Upstream: `alt+p` on Windows, `shift+ctrl+p` elsewhere.
                default_keys: vec![if cfg!(windows) {
                    KeyCombo::new(Char('p'), M::ALT)
                } else {
                    KeyCombo::new(Char('p'), M::CONTROL | M::SHIFT)
                }],
                description: Some("Cycle to previous model"),
            },
        );
        map.insert(
            "app.tools.expand",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('o'), M::CONTROL)],
                description: Some("Toggle tool output"),
            },
        );
        map.insert(
            "app.thinking.toggle",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('t'), M::CONTROL)],
                description: Some("Toggle thinking blocks"),
            },
        );
        map.insert(
            "app.editor.external",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('g'), M::CONTROL)],
                description: Some("Open external editor"),
            },
        );
        map.insert(
            "app.thinking.cycle",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Tab, M::SHIFT)],
                description: Some("Cycle thinking level"),
            },
        );
        map.insert(
            "app.clipboard.pasteImage",
            KeybindingDefinition {
                // Upstream: `alt+v` on Windows, `ctrl+v` elsewhere. rpi keeps
                // `ctrl+v` on Windows too — its clipboard *text* fallback
                // (crossterm never emits `Event::Paste` on the Windows console)
                // rides the same binding, so native's key is added, not swapped.
                default_keys: if cfg!(windows) {
                    vec![
                        KeyCombo::new(Char('v'), M::CONTROL),
                        KeyCombo::new(Char('v'), M::ALT),
                    ]
                } else {
                    vec![KeyCombo::new(Char('v'), M::CONTROL)]
                },
                description: Some("Paste image from clipboard"),
            },
        );
        map.insert(
            "app.suspend",
            KeybindingDefinition {
                // Upstream binds no key on Windows at all.
                default_keys: if cfg!(windows) {
                    vec![]
                } else {
                    vec![KeyCombo::new(Char('z'), M::CONTROL)]
                },
                description: Some("Suspend to background"),
            },
        );
        map.insert(
            "app.message.followUp",
            KeybindingDefinition {
                // Upstream: `ctrl+q` on Windows, `alt+enter` elsewhere. rpi
                // keeps `alt+enter` on Windows as well (it already worked) and
                // adds native's key alongside it.
                default_keys: if cfg!(windows) {
                    vec![
                        KeyCombo::new(Char('q'), M::CONTROL),
                        KeyCombo::new(Enter, M::ALT),
                    ]
                } else {
                    vec![KeyCombo::new(Enter, M::ALT)]
                },
                description: Some("Queue follow-up message"),
            },
        );
        map.insert(
            "app.message.dequeue",
            KeybindingDefinition {
                // Upstream: `alt+q` on Windows, `alt+up` elsewhere.
                default_keys: vec![if cfg!(windows) {
                    KeyCombo::new(Char('q'), M::ALT)
                } else {
                    KeyCombo::new(Up, M::ALT)
                }],
                description: Some("Restore queued messages"),
            },
        );
        map.insert(
            "app.message.copy",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('x'), M::CONTROL)],
                description: Some("Copy selection or last assistant message"),
            },
        );

        // Editor navigation and editing
        map.insert(
            "tui.editor.cursorUp",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Up, M::NONE)],
                description: Some("Move cursor up"),
            },
        );
        map.insert(
            "tui.editor.cursorDown",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Down, M::NONE)],
                description: Some("Move cursor down"),
            },
        );
        map.insert(
            "tui.editor.historyPrevious",
            KeybindingDefinition {
                default_keys: vec![],
                description: Some("Select previous prompt history entry"),
            },
        );
        map.insert(
            "tui.editor.historyNext",
            KeybindingDefinition {
                default_keys: vec![],
                description: Some("Select next prompt history entry"),
            },
        );
        map.insert(
            "tui.editor.cursorLeft",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Left, M::NONE),
                    KeyCombo::new(Char('b'), M::CONTROL),
                ],
                description: Some("Move cursor left"),
            },
        );
        map.insert(
            "tui.editor.cursorRight",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Right, M::NONE),
                    KeyCombo::new(Char('f'), M::CONTROL),
                ],
                description: Some("Move cursor right"),
            },
        );
        map.insert(
            "tui.editor.cursorWordLeft",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Left, M::ALT),
                    KeyCombo::new(Left, M::CONTROL),
                    KeyCombo::new(Char('b'), M::ALT),
                ],
                description: Some("Move cursor word left"),
            },
        );
        map.insert(
            "tui.editor.cursorWordRight",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Right, M::ALT),
                    KeyCombo::new(Right, M::CONTROL),
                    KeyCombo::new(Char('f'), M::ALT),
                ],
                description: Some("Move cursor word right"),
            },
        );
        map.insert(
            "tui.editor.cursorLineStart",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Home, M::NONE),
                    KeyCombo::new(Home, M::CONTROL),
                    KeyCombo::new(Char('a'), M::CONTROL),
                ],
                description: Some("Move to line start"),
            },
        );
        map.insert(
            "tui.editor.cursorLineEnd",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(End, M::NONE),
                    KeyCombo::new(End, M::CONTROL),
                    KeyCombo::new(Char('e'), M::CONTROL),
                ],
                description: Some("Move to line end"),
            },
        );
        map.insert(
            "tui.editor.jumpForward",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char(']'), M::CONTROL)],
                description: Some("Jump forward to character"),
            },
        );
        map.insert(
            "tui.editor.jumpBackward",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char(']'), M::CONTROL | M::ALT)],
                description: Some("Jump backward to character"),
            },
        );
        map.insert(
            "tui.editor.pageUp",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(PageUp, M::NONE),
                    KeyCombo::new(PageUp, M::CONTROL),
                ],
                description: Some("Page up"),
            },
        );
        map.insert(
            "tui.editor.pageDown",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(PageDown, M::NONE),
                    KeyCombo::new(PageDown, M::CONTROL),
                ],
                description: Some("Page down"),
            },
        );
        map.insert(
            "tui.editor.deleteCharBackward",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Backspace, M::NONE)],
                description: Some("Delete character backward"),
            },
        );
        map.insert(
            "tui.editor.deleteCharForward",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Delete, M::NONE),
                    KeyCombo::new(Char('d'), M::CONTROL),
                ],
                description: Some("Delete character forward"),
            },
        );
        map.insert(
            "tui.editor.deleteWordBackward",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Char('w'), M::CONTROL),
                    KeyCombo::new(Backspace, M::ALT),
                ],
                description: Some("Delete word backward"),
            },
        );
        map.insert(
            "tui.editor.deleteWordForward",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Char('d'), M::ALT),
                    KeyCombo::new(Delete, M::ALT),
                ],
                description: Some("Delete word forward"),
            },
        );
        map.insert(
            "tui.editor.deleteToLineStart",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('u'), M::CONTROL)],
                description: Some("Delete to line start"),
            },
        );
        map.insert(
            "tui.editor.deleteToLineEnd",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('k'), M::CONTROL)],
                description: Some("Delete to line end"),
            },
        );
        map.insert(
            "tui.editor.yank",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('y'), M::CONTROL)],
                description: Some("Yank"),
            },
        );
        map.insert(
            "tui.editor.yankPop",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('y'), M::ALT)],
                description: Some("Yank pop"),
            },
        );
        map.insert(
            "tui.editor.undo",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('-'), M::CONTROL)],
                description: Some("Undo"),
            },
        );

        // Generic input actions
        map.insert(
            "tui.input.newLine",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Enter, M::SHIFT),
                    KeyCombo::new(Char('j'), M::CONTROL),
                ],
                description: Some("Insert newline"),
            },
        );
        map.insert(
            "tui.input.submit",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Enter, M::NONE)],
                description: Some("Submit input"),
            },
        );
        map.insert(
            "tui.input.tab",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Tab, M::NONE)],
                description: Some("Tab / autocomplete"),
            },
        );
        map.insert(
            "tui.input.copy",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('c'), M::CONTROL)],
                description: Some("Copy selection"),
            },
        );

        // Generic selection actions
        map.insert(
            "tui.select.up",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Up, M::NONE)],
                description: Some("Move selection up"),
            },
        );
        map.insert(
            "tui.select.down",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Down, M::NONE)],
                description: Some("Move selection down"),
            },
        );
        map.insert(
            "tui.select.pageUp",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(PageUp, M::NONE)],
                description: Some("Selection page up"),
            },
        );
        map.insert(
            "tui.select.pageDown",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(PageDown, M::NONE)],
                description: Some("Selection page down"),
            },
        );
        map.insert(
            "tui.select.confirm",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Enter, M::NONE)],
                description: Some("Confirm selection"),
            },
        );
        map.insert(
            "tui.select.cancel",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Esc, M::NONE),
                    KeyCombo::new(Char('c'), M::CONTROL),
                ],
                description: Some("Cancel selection"),
            },
        );

        // Alternate-screen viewport navigation
        map.insert(
            "tui.altScreen.pageUp",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(PageUp, M::NONE)],
                description: Some("Scroll viewport up one page"),
            },
        );
        map.insert(
            "tui.altScreen.pageDown",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(PageDown, M::NONE)],
                description: Some("Scroll viewport down one page"),
            },
        );
        map.insert(
            "tui.altScreen.halfPageUp",
            KeybindingDefinition {
                default_keys: vec![],
                description: Some("Scroll viewport up half a page"),
            },
        );
        map.insert(
            "tui.altScreen.halfPageDown",
            KeybindingDefinition {
                default_keys: vec![],
                description: Some("Scroll viewport down half a page"),
            },
        );
        map.insert(
            "tui.altScreen.lineUp",
            KeybindingDefinition {
                default_keys: vec![],
                description: Some("Scroll viewport up one line"),
            },
        );
        map.insert(
            "tui.altScreen.lineDown",
            KeybindingDefinition {
                default_keys: vec![],
                description: Some("Scroll viewport down one line"),
            },
        );
        map.insert(
            "tui.altScreen.previousPrompt",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Up, M::CONTROL | M::SHIFT)],
                description: Some("Jump to previous semantic prompt"),
            },
        );
        map.insert(
            "tui.altScreen.nextPrompt",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Down, M::CONTROL | M::SHIFT)],
                description: Some("Jump to next semantic prompt"),
            },
        );
        map.insert(
            "tui.altScreen.search",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Char('f'), M::CONTROL | M::SHIFT)],
                description: Some("Search the primary scroll view"),
            },
        );
        map.insert(
            "tui.altScreen.searchNext",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Enter, M::NONE),
                    KeyCombo::new(Char('g'), M::CONTROL),
                ],
                description: Some("Select the next search match"),
            },
        );
        map.insert(
            "tui.altScreen.searchPrevious",
            KeybindingDefinition {
                default_keys: vec![
                    KeyCombo::new(Enter, M::SHIFT),
                    KeyCombo::new(Char('g'), M::CONTROL | M::SHIFT),
                ],
                description: Some("Select the previous search match"),
            },
        );
        map.insert(
            "tui.altScreen.searchClose",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Esc, M::NONE)],
                description: Some("Close transcript search"),
            },
        );
        map.insert(
            "tui.altScreen.top",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(Home, M::NONE)],
                description: Some("Scroll viewport to top"),
            },
        );
        map.insert(
            "tui.altScreen.bottom",
            KeybindingDefinition {
                default_keys: vec![KeyCombo::new(End, M::NONE)],
                description: Some("Scroll viewport to bottom"),
            },
        );

        map
    }

    /// Check if a key event matches a keybinding.
    pub fn matches(&self, event: &crossterm::event::KeyEvent, binding_id: KeybindingId) -> bool {
        if let Some(combos) = self.keys_by_id.get(binding_id) {
            let combo = KeyCombo::new(event.code, event.modifiers);
            return combos.contains(&combo);
        }
        false
    }

    /// Get the keybinding ID for a key event.
    pub fn get_binding(&self, event: &crossterm::event::KeyEvent) -> Option<KeybindingId> {
        let combo = KeyCombo::new(event.code, event.modifiers);
        for (id, combos) in &self.keys_by_id {
            if combos.contains(&combo) {
                return Some(*id);
            }
        }
        None
    }

    /// Get keys for a keybinding.
    pub fn get_keys(&self, binding_id: KeybindingId) -> Vec<KeyCombo> {
        self.keys_by_id.get(binding_id).cloned().unwrap_or_default()
    }

    /// Get definition for a keybinding.
    pub fn get_definition(&self, binding_id: KeybindingId) -> Option<&KeybindingDefinition> {
        self.definitions.get(binding_id)
    }

    /// Get conflicts.
    pub fn get_conflicts(&self) -> &[KeybindingConflict] {
        &self.conflicts
    }

    /// Set user bindings.
    pub fn set_user_bindings(&mut self, user_bindings: HashMap<KeybindingId, Vec<KeyCombo>>) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    /// Rebuild the keybindings after user bindings change.
    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        // Track which keys are claimed by which keybindings
        let mut key_claims: HashMap<KeyCombo, Vec<KeybindingId>> = HashMap::new();

        for (id, def) in &self.definitions {
            let keys = if let Some(user_keys) = self.user_bindings.get(id) {
                user_keys.clone()
            } else {
                def.default_keys.clone()
            };

            // Track claims
            for key in &keys {
                key_claims.entry(*key).or_default().push(*id);
            }

            self.keys_by_id.insert(*id, keys);
        }

        // Find conflicts
        for (key, claimants) in key_claims {
            if claimants.len() > 1 {
                self.conflicts.push(KeybindingConflict {
                    key,
                    keybindings: claimants,
                });
            }
        }
    }

    /// Set a single keybinding.
    pub fn set(&mut self, binding_id: KeybindingId, combos: Vec<KeyCombo>) {
        self.user_bindings.insert(binding_id, combos);
        self.rebuild();
    }
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::new()
    }
}

/// Global keybindings manager.
static GLOBAL_KEYBINDINGS: std::sync::OnceLock<std::sync::Mutex<Keybindings>> =
    std::sync::OnceLock::new();

/// Set global keybindings.
pub fn set_keybindings(keybindings: Keybindings) {
    let guard = GLOBAL_KEYBINDINGS.get_or_init(|| std::sync::Mutex::new(Keybindings::new()));
    if let Ok(mut kb) = guard.lock() {
        *kb = keybindings;
    }
}

/// Get global keybindings.
pub fn get_keybindings() -> std::sync::MutexGuard<'static, Keybindings> {
    let guard = GLOBAL_KEYBINDINGS.get_or_init(|| std::sync::Mutex::new(Keybindings::new()));
    guard.lock().expect("Failed to lock global keybindings")
}

/// Predefined keybinding IDs.
pub mod keys {
    use super::KeybindingId;

    pub const INTERRUPT: KeybindingId = "app.interrupt";
    pub const CLEAR: KeybindingId = "app.clear";
    pub const EXIT: KeybindingId = "app.exit";
    pub const MODEL_SELECT: KeybindingId = "app.model.select";
    pub const MODEL_CYCLE_FORWARD: KeybindingId = "app.model.cycleForward";
    pub const MODEL_CYCLE_BACKWARD: KeybindingId = "app.model.cycleBackward";
    pub const TOOLS_EXPAND: KeybindingId = "app.tools.expand";
    pub const THINKING_TOGGLE: KeybindingId = "app.thinking.toggle";
    pub const EXTERNAL_EDITOR: KeybindingId = "app.editor.external";
    pub const THINKING_CYCLE: KeybindingId = "app.thinking.cycle";
    pub const PASTE_IMAGE: KeybindingId = "app.clipboard.pasteImage";
    pub const SUSPEND: KeybindingId = "app.suspend";
    pub const MESSAGE_FOLLOW_UP: KeybindingId = "app.message.followUp";
    pub const DEQUEUE: KeybindingId = "app.message.dequeue";
    pub const MESSAGE_COPY: KeybindingId = "app.message.copy";

    // Editor
    pub const CURSOR_UP: KeybindingId = "tui.editor.cursorUp";
    pub const CURSOR_DOWN: KeybindingId = "tui.editor.cursorDown";
    pub const CURSOR_LEFT: KeybindingId = "tui.editor.cursorLeft";
    pub const CURSOR_RIGHT: KeybindingId = "tui.editor.cursorRight";
    pub const CURSOR_WORD_LEFT: KeybindingId = "tui.editor.cursorWordLeft";
    pub const CURSOR_WORD_RIGHT: KeybindingId = "tui.editor.cursorWordRight";
    pub const CURSOR_LINE_START: KeybindingId = "tui.editor.cursorLineStart";
    pub const CURSOR_LINE_END: KeybindingId = "tui.editor.cursorLineEnd";
    pub const DELETE_CHAR_BACKWARD: KeybindingId = "tui.editor.deleteCharBackward";
    pub const DELETE_CHAR_FORWARD: KeybindingId = "tui.editor.deleteCharForward";
    pub const DELETE_WORD_BACKWARD: KeybindingId = "tui.editor.deleteWordBackward";
    pub const DELETE_WORD_FORWARD: KeybindingId = "tui.editor.deleteWordForward";
    pub const DELETE_TO_LINE_START: KeybindingId = "tui.editor.deleteToLineStart";
    pub const DELETE_TO_LINE_END: KeybindingId = "tui.editor.deleteToLineEnd";
    pub const YANK: KeybindingId = "tui.editor.yank";
    pub const YANK_POP: KeybindingId = "tui.editor.yankPop";
    pub const UNDO: KeybindingId = "tui.editor.undo";

    // Input
    pub const INPUT_SUBMIT: KeybindingId = "tui.input.submit";
    pub const INPUT_NEW_LINE: KeybindingId = "tui.input.newLine";
    pub const INPUT_TAB: KeybindingId = "tui.input.tab";
    pub const INPUT_COPY: KeybindingId = "tui.input.copy";

    // Select
    pub const SELECT_UP: KeybindingId = "tui.select.up";
    pub const SELECT_DOWN: KeybindingId = "tui.select.down";
    pub const SELECT_PAGE_UP: KeybindingId = "tui.select.pageUp";
    pub const SELECT_PAGE_DOWN: KeybindingId = "tui.select.pageDown";
    pub const SELECT_CONFIRM: KeybindingId = "tui.select.confirm";
    pub const SELECT_CANCEL: KeybindingId = "tui.select.cancel";

    // Alt screen
    pub const PAGE_UP: KeybindingId = "tui.altScreen.pageUp";
    pub const PAGE_DOWN: KeybindingId = "tui.altScreen.pageDown";
    pub const HALF_PAGE_UP: KeybindingId = "tui.altScreen.halfPageUp";
    pub const HALF_PAGE_DOWN: KeybindingId = "tui.altScreen.halfPageDown";
    pub const LINE_UP: KeybindingId = "tui.altScreen.lineUp";
    pub const LINE_DOWN: KeybindingId = "tui.altScreen.lineDown";
    pub const TOP: KeybindingId = "tui.altScreen.top";
    pub const BOTTOM: KeybindingId = "tui.altScreen.bottom";
    pub const SEARCH: KeybindingId = "tui.altScreen.search";
    pub const SEARCH_NEXT: KeybindingId = "tui.altScreen.searchNext";
    pub const SEARCH_PREVIOUS: KeybindingId = "tui.altScreen.searchPrevious";
    pub const SEARCH_CLOSE: KeybindingId = "tui.altScreen.searchClose";
    pub const PREVIOUS_PROMPT: KeybindingId = "tui.altScreen.previousPrompt";
    pub const NEXT_PROMPT: KeybindingId = "tui.altScreen.nextPrompt";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keybindings_new() {
        let kb = Keybindings::new();
        assert!(kb.definitions.len() > 0);
    }

    #[test]
    fn test_matches() {
        let kb = Keybindings::new();
        let event = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(kb.matches(&event, keys::INPUT_SUBMIT));
    }

    #[test]
    fn test_get_keys() {
        let kb = Keybindings::new();
        let keys = kb.get_keys(keys::CURSOR_LEFT);
        assert!(!keys.is_empty());
    }

    #[test]
    fn key_combo_display_formats_hints() {
        assert_eq!(
            KeyCombo::new(KeyCode::Char('q'), KeyModifiers::ALT).display(),
            "Alt+Q"
        );
        assert_eq!(
            KeyCombo::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
            .display(),
            "Ctrl+Shift+F"
        );
        assert_eq!(
            KeyCombo::new(KeyCode::Enter, KeyModifiers::NONE).display(),
            "Enter"
        );
        assert_eq!(
            KeyCombo::new(KeyCode::Up, KeyModifiers::ALT).display(),
            "Alt+Up"
        );
    }

    #[test]
    fn dequeue_binding_has_a_platform_default() {
        let kb = Keybindings::new();
        let keys = kb.get_keys(keys::DEQUEUE);
        assert_eq!(keys.len(), 1);
        if cfg!(windows) {
            assert_eq!(keys[0].display(), "Alt+Q");
        } else {
            assert_eq!(keys[0].display(), "Alt+Up");
        }
    }

    #[test]
    fn dequeue_matches_its_platform_default_event() {
        // Guards the hotkey itself: a real Alt+Q (Windows) / Alt+Up (elsewhere)
        // event must resolve to `app.message.dequeue`, since the TUI key loop
        // routes the "edit queued messages" action solely through this match.
        let kb = Keybindings::new();
        let event = if cfg!(windows) {
            crossterm::event::KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)
        } else {
            crossterm::event::KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)
        };
        assert!(kb.matches(&event, keys::DEQUEUE));
    }
}
