//! Settings list component.
//!
//! A list of settings with navigation and in-place value cycling. Port of
//! upstream's `components/settings-list.ts`: Up/Down navigate, Enter (or
//! Space) activates the row — cycling to the next value, or invoking the
//! `submenu` callback for rows that open a nested selector — and Esc cancels.

use std::any::Any;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent};

use super::component::Component;
use super::fuzzy::fuzzy_filter;
use crate::utils::{truncate_to_width, visible_width};

/// Width of the `"→ "` / `"  "` row prefix.
const PREFIX_WIDTH: usize = 2;
/// Width reserved for the value column so labels cannot crowd it out.
const VALUE_COLUMN_WIDTH: usize = 15;
/// Narrowest label column worth rendering.
const MIN_LABEL_COLUMN_WIDTH: usize = 20;
/// Widest label column; beyond this a long label is truncated with `…` so the
/// description keeps room on wide terminals.
const MAX_LABEL_COLUMN_WIDTH: usize = 32;

/// A setting item.
#[derive(Debug, Clone)]
pub struct SettingItem {
    /// Setting key.
    pub key: String,
    /// Display label.
    pub label: String,
    /// Setting value.
    pub value: String,
    /// Description.
    pub description: Option<String>,
    /// Setting type.
    pub setting_type: SettingType,
    /// Values cycled through by [`SettingsList::activate`]. Empty for submenu
    /// triggers and read-only rows.
    pub values: Vec<String>,
    /// Whether activating the row should call `on_select` (open a submenu)
    /// rather than cycle `values`.
    pub has_submenu: bool,
}

impl SettingItem {
    /// Create a new setting item.
    pub fn new(key: &str, label: &str, value: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            value: value.to_string(),
            description: None,
            setting_type: SettingType::String,
            values: Vec::new(),
            has_submenu: false,
        }
    }

    /// Add description.
    pub fn with_description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    /// Set setting type.
    pub fn with_type(mut self, setting_type: SettingType) -> Self {
        self.setting_type = setting_type;
        self
    }

    /// The cycle order for this row. Activating the row advances to the next
    /// value and fires `on_change`. Empty ⇒ the row is a submenu trigger or a
    /// read-only display.
    pub fn with_values(mut self, values: &[&str]) -> Self {
        self.values = values.iter().map(|v| v.to_string()).collect();
        self
    }

    /// Mark the row as a submenu trigger: activating it calls `on_select`
    /// instead of cycling values.
    pub fn with_submenu(mut self) -> Self {
        self.has_submenu = true;
        self
    }
}

/// Setting type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingType {
    /// String setting.
    String,
    /// Boolean setting.
    Boolean,
    /// Number setting.
    Number,
    /// Enum setting.
    Enum,
}

/// Settings list theme.
#[derive(Debug, Clone)]
pub struct SettingsListTheme {
    pub key: fn(&str) -> String,
    pub value: fn(&str) -> String,
    pub description: fn(&str) -> String,
    pub selected: fn(&str) -> String,
    pub selected_prefix: fn(&str) -> String,
}

impl Default for SettingsListTheme {
    fn default() -> Self {
        Self {
            key: |s| format!("\x1b[1m{}\x1b[0m", s),
            value: |s| format!("\x1b[36m{}\x1b[0m", s),
            description: |s| format!("\x1b[90m{}\x1b[0m", s),
            selected: |s| format!("\x1b[7m{}\x1b[0m", s),
            selected_prefix: |s| format!("\x1b[32m{}\x1b[0m", s),
        }
    }
}

/// Settings list component.
pub struct SettingsList {
    settings: Mutex<Vec<SettingItem>>,
    filtered: Mutex<Vec<SettingItem>>,
    selected_index: Mutex<usize>,
    max_visible: usize,
    theme: SettingsListTheme,
    filter: Mutex<String>,
    on_change: Mutex<Option<Arc<dyn Fn(&str, &str) + Send + Sync>>>,
    on_select: Mutex<Option<Arc<dyn Fn(&SettingItem) + Send + Sync>>>,
    on_cancel: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl SettingsList {
    /// Create a new settings list.
    pub fn new(settings: Vec<SettingItem>) -> Self {
        Self {
            filtered: Mutex::new(settings.clone()),
            settings: Mutex::new(settings),
            selected_index: Mutex::new(0),
            max_visible: 10,
            theme: SettingsListTheme::default(),
            filter: Mutex::new(String::new()),
            on_change: Mutex::new(None),
            on_select: Mutex::new(None),
            on_cancel: Mutex::new(None),
        }
    }

    /// Create with theme.
    pub fn with_theme(settings: Vec<SettingItem>, theme: SettingsListTheme) -> Self {
        Self {
            filtered: Mutex::new(settings.clone()),
            settings: Mutex::new(settings),
            selected_index: Mutex::new(0),
            max_visible: 10,
            theme,
            filter: Mutex::new(String::new()),
            on_change: Mutex::new(None),
            on_select: Mutex::new(None),
            on_cancel: Mutex::new(None),
        }
    }

    /// Set filter.
    pub fn set_filter(&self, filter: &str) {
        if let Ok(mut f) = self.filter.lock() {
            *f = filter.to_string();
        }
        self.refilter();
        // A changed query resets the cursor to the first match.
        if let Ok(mut idx) = self.selected_index.lock() {
            *idx = 0;
        }
    }

    /// Rebuild the visible rows from the current filter without moving the
    /// cursor (a value change must not jump the selection back to the top).
    fn refilter(&self) {
        let filter = self.filter.lock().map(|f| f.clone()).unwrap_or_default();
        if let Ok(settings) = self.settings.lock() {
            let filtered = fuzzy_filter(&settings, &filter, |item| item.key.as_str());
            if let Ok(mut f) = self.filtered.lock() {
                *f = filtered;
            }
        }
    }

    /// Get selected item.
    pub fn get_selected(&self) -> Option<SettingItem> {
        let idx = *self.selected_index.lock().ok()?;
        let filtered = self.filtered.lock().ok()?;
        filtered.get(idx).cloned()
    }

    /// Set selected index.
    pub fn set_selected(&self, index: usize) {
        if let Ok(mut idx) = self.selected_index.lock() {
            if let Ok(filtered) = self.filtered.lock() {
                *idx = index.min(filtered.len().saturating_sub(1));
            }
        }
    }

    /// Move selection up.
    pub fn move_up(&self) {
        if let Ok(mut idx) = self.selected_index.lock() {
            if let Ok(filtered) = self.filtered.lock() {
                if !filtered.is_empty() {
                    if *idx == 0 {
                        *idx = filtered.len() - 1;
                    } else {
                        *idx -= 1;
                    }
                }
            }
        }
    }

    /// Move selection down.
    pub fn move_down(&self) {
        if let Ok(mut idx) = self.selected_index.lock() {
            if let Ok(filtered) = self.filtered.lock() {
                if !filtered.is_empty() {
                    if *idx >= filtered.len() - 1 {
                        *idx = 0;
                    } else {
                        *idx += 1;
                    }
                }
            }
        }
    }

    /// Toggle boolean setting.
    pub fn toggle(&self) {
        if let Some(item) = self.get_selected() {
            if item.setting_type == SettingType::Boolean {
                let new_value = if item.value == "true" {
                    "false"
                } else {
                    "true"
                };
                self.update_value(&item.key, new_value);

                if let Ok(cb) = self.on_change.lock() {
                    if let Some(callback) = cb.as_ref() {
                        callback(&item.key, new_value);
                    }
                }
            }
        }
    }

    /// Update a setting value.
    pub fn update_value(&self, key: &str, value: &str) {
        if let Ok(mut settings) = self.settings.lock() {
            for setting in settings.iter_mut() {
                if setting.key == key {
                    setting.value = value.to_string();
                    break;
                }
            }
        }

        // Refresh the visible rows but keep the cursor where the user was.
        self.refilter();
        if let Ok(mut idx) = self.selected_index.lock() {
            if let Ok(filtered) = self.filtered.lock() {
                *idx = (*idx).min(filtered.len().saturating_sub(1));
            }
        }
    }

    /// Set change callback.
    pub fn on_change(&self, callback: Arc<dyn Fn(&str, &str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_change.lock() {
            *cb = Some(callback);
        }
    }

    /// Set select callback.
    pub fn on_select(&self, callback: Arc<dyn Fn(&SettingItem) + Send + Sync>) {
        if let Ok(mut cb) = self.on_select.lock() {
            *cb = Some(callback);
        }
    }

    /// Set cancel callback (Esc). Upstream's `SettingsList.onCancel`.
    pub fn on_cancel(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut cb) = self.on_cancel.lock() {
            *cb = Some(callback);
        }
    }

    /// Fire the cancel callback.
    pub fn cancel(&self) {
        if let Ok(cb) = self.on_cancel.lock() {
            if let Some(callback) = cb.as_ref() {
                callback();
            }
        }
    }

    /// Activate the selected row: open its submenu, or cycle to the next value.
    /// Mirrors upstream's `SettingsList.activateItem`.
    ///
    /// Returns the new value when a value was cycled.
    pub fn activate(&self) -> Option<(String, String)> {
        let item = self.get_selected()?;
        if item.has_submenu {
            if let Ok(cb) = self.on_select.lock() {
                if let Some(callback) = cb.as_ref() {
                    callback(&item);
                }
            }
            return None;
        }
        if item.values.is_empty() {
            return None;
        }
        let current = item
            .values
            .iter()
            .position(|value| value == &item.value)
            .unwrap_or(item.values.len().saturating_sub(1));
        let next = item.values[(current + 1) % item.values.len()].clone();
        self.update_value(&item.key, &next);
        if let Ok(cb) = self.on_change.lock() {
            if let Some(callback) = cb.as_ref() {
                callback(&item.key, &next);
            }
        }
        Some((item.key, next))
    }

    /// Route a key: Up/Down navigate, Enter or Space activates, Esc cancels.
    /// Mirrors upstream's `SettingsList.handleInput`.
    pub fn handle_key(&self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Enter => {
                self.activate();
            }
            KeyCode::Char(' ') => {
                self.activate();
            }
            KeyCode::Esc => self.cancel(),
            _ => {}
        }
    }

    /// The current value of `key`, if the row exists.
    pub fn value_of(&self, key: &str) -> Option<String> {
        let settings = self.settings.lock().ok()?;
        settings
            .iter()
            .find(|item| item.key == key)
            .map(|item| item.value.clone())
    }

    /// Replace every row (used to rebuild after an external settings change
    /// without reopening the selector).
    pub fn set_items(&self, settings: Vec<SettingItem>) {
        let filter = self.filter.lock().map(|f| f.clone()).unwrap_or_default();
        if let Ok(mut guard) = self.settings.lock() {
            *guard = settings;
        }
        self.set_filter(&filter);
    }

    /// Confirm selection.
    pub fn confirm(&self) {
        if let Some(item) = self.get_selected() {
            if let Ok(cb) = self.on_select.lock() {
                if let Some(callback) = cb.as_ref() {
                    callback(&item);
                }
            }
        }
    }

    /// Width of the label column shared by every rendered row.
    ///
    /// Wide enough for the longest label in `filtered` so labels are never cut
    /// mid-word, but clamped to `[MIN_LABEL_COLUMN_WIDTH, MAX_LABEL_COLUMN_WIDTH]`
    /// and to the space left by the value column, so a narrow terminal still
    /// shows the values.
    fn label_column_width(&self, filtered: &[SettingItem], width: usize) -> usize {
        let reserved = PREFIX_WIDTH + VALUE_COLUMN_WIDTH + 1;
        let available = width.saturating_sub(reserved);
        let widest = filtered
            .iter()
            .map(|item| visible_width(&item.label))
            .max()
            .unwrap_or(MIN_LABEL_COLUMN_WIDTH);
        widest
            .clamp(MIN_LABEL_COLUMN_WIDTH, MAX_LABEL_COLUMN_WIDTH)
            .min(available)
            .max(1)
    }

    /// Render a single setting item.
    ///
    /// `label_width` is the column width from [`Self::label_column_width`]; the
    /// label is padded to it so every value starts in the same column.
    fn render_item(
        &self,
        item: &SettingItem,
        is_selected: bool,
        width: usize,
        label_width: usize,
    ) -> String {
        let prefix = if is_selected { "→ " } else { "  " };

        let value_width = VALUE_COLUMN_WIDTH;

        let label = truncate_to_width(&item.label, label_width, "…");
        let label_pad = " ".repeat(label_width.saturating_sub(visible_width(&label)));
        let key_display = format!("{label}{label_pad}");
        let value_display = truncate_to_width(&item.value, value_width, "");

        let main_line = if is_selected {
            format!(
                "{}{} {}",
                (self.theme.selected_prefix)(prefix),
                (self.theme.key)(&key_display),
                (self.theme.selected)(&value_display)
            )
        } else {
            format!(
                "{}{} {}",
                prefix,
                (self.theme.key)(&key_display),
                (self.theme.value)(&value_display)
            )
        };

        // Add description if available
        if let Some(ref desc) = item.description {
            let desc_width = width.saturating_sub(visible_width(&main_line) + 2);
            if desc_width > 10 {
                let desc_display = truncate_to_width(desc, desc_width, "");
                format!("{} {}", main_line, (self.theme.description)(&desc_display))
            } else {
                main_line
            }
        } else {
            main_line
        }
    }
}

impl Component for SettingsList {
    fn render(&self, width: usize) -> Vec<String> {
        let filtered = if let Ok(f) = self.filtered.lock() {
            f.clone()
        } else {
            return Vec::new();
        };

        let selected = *self.selected_index.lock().unwrap();

        if filtered.is_empty() {
            return vec![(self.theme.description)("  No settings found")];
        }

        // Calculate visible range
        let start = if filtered.len() <= self.max_visible {
            0
        } else {
            selected
                .saturating_sub(self.max_visible / 2)
                .min(filtered.len().saturating_sub(self.max_visible))
        };
        let end = (start + self.max_visible).min(filtered.len());

        // One label column for the whole list so values line up.
        let label_width = self.label_column_width(&filtered, width);

        let mut lines = Vec::new();

        for i in start..end {
            if let Some(item) = filtered.get(i) {
                lines.push(self.render_item(item, i == selected, width, label_width));
            }
        }

        // Add scroll indicator
        if start > 0 || end < filtered.len() {
            let info = format!("  ({}/{})", selected + 1, filtered.len());
            lines.push((self.theme.description)(&info));
        }

        lines
    }

    fn invalidate(&self) {
        // No cached state
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_list_new() {
        let settings = vec![
            SettingItem::new("theme", "Theme", "dark"),
            SettingItem::new("font", "Font Size", "14"),
        ];
        let list = SettingsList::new(settings);

        let lines = list.render(60);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_settings_navigation() {
        let settings = vec![
            SettingItem::new("a", "A", "1"),
            SettingItem::new("b", "B", "2"),
        ];
        let list = SettingsList::new(settings);

        assert_eq!(list.get_selected().unwrap().key, "a");
        list.move_down();
        assert_eq!(list.get_selected().unwrap().key, "b");
        list.move_down(); // Wrap around
        assert_eq!(list.get_selected().unwrap().key, "a");
    }

    #[test]
    fn test_settings_filter() {
        let settings = vec![
            SettingItem::new("theme", "Theme", "dark"),
            SettingItem::new("font", "Font Size", "14"),
        ];
        let list = SettingsList::new(settings);

        list.set_filter("theme");
        let selected = list.get_selected();
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().key, "theme");
    }

    #[test]
    fn activate_cycles_values_and_fires_change() {
        let settings = vec![SettingItem::new("hide-thinking", "Hide thinking", "false")
            .with_values(&["true", "false"])];
        let list = SettingsList::new(settings);

        let changes: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let changes_cb = changes.clone();
        list.on_change(Arc::new(move |key, value| {
            changes_cb
                .lock()
                .unwrap()
                .push((key.to_string(), value.to_string()));
        }));

        // current "false" -> next in cycle is "true"
        assert_eq!(
            list.activate(),
            Some(("hide-thinking".to_string(), "true".to_string()))
        );
        assert_eq!(list.value_of("hide-thinking").as_deref(), Some("true"));
        // Cycling wraps back around.
        list.activate();
        assert_eq!(list.value_of("hide-thinking").as_deref(), Some("false"));
        assert_eq!(changes.lock().unwrap().len(), 2);
    }

    #[test]
    fn activating_a_value_preserves_the_cursor() {
        let settings = vec![
            SettingItem::new("a", "Auto-compact", "true").with_values(&["true", "false"]),
            SettingItem::new("b", "Steering mode", "all").with_values(&["all", "one-at-a-time"]),
            SettingItem::new("c", "Quiet startup", "false").with_values(&["true", "false"]),
        ];
        let list = SettingsList::new(settings);
        list.move_down();
        assert_eq!(list.get_selected().unwrap().key, "b");
        list.activate();
        // Upstream keeps the cursor on the row the user just changed.
        assert_eq!(list.get_selected().unwrap().key, "b");
        assert_eq!(
            list.get_selected().unwrap().value,
            "one-at-a-time",
            "value should have cycled in place"
        );
    }

    #[test]
    fn submenu_row_calls_select_instead_of_cycling() {
        let settings = vec![SettingItem::new("theme", "Theme", "dark").with_submenu()];
        let list = SettingsList::new(settings);
        let opened: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let opened_cb = opened.clone();
        list.on_select(Arc::new(move |item| {
            opened_cb.lock().unwrap().push(item.key.clone());
        }));
        assert_eq!(list.activate(), None);
        assert_eq!(opened.lock().unwrap().as_slice(), ["theme"]);
        // Value untouched.
        assert_eq!(list.value_of("theme").as_deref(), Some("dark"));
    }

    #[test]
    fn handle_key_routes_navigation_activation_and_cancel() {
        use crossterm::event::{KeyCode, KeyEvent};
        let settings = vec![
            SettingItem::new("a", "Alpha", "1").with_values(&["1", "2"]),
            SettingItem::new("b", "Beta", "x").with_values(&["x", "y"]),
        ];
        let list = SettingsList::new(settings);
        let cancelled = Arc::new(Mutex::new(false));
        let cancelled_cb = cancelled.clone();
        list.on_cancel(Arc::new(move || {
            *cancelled_cb.lock().unwrap() = true;
        }));

        list.handle_key(KeyEvent::new(
            KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(list.get_selected().unwrap().key, "b");
        list.handle_key(KeyEvent::new(
            KeyCode::Char(' '),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(list.value_of("b").as_deref(), Some("y"));
        list.handle_key(KeyEvent::new(
            KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(*cancelled.lock().unwrap());
    }
}
