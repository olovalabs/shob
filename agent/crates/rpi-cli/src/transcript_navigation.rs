//! Transcript navigation shared by the local and driver-based TUI.
use std::sync::Arc;

use crossterm::event::KeyEvent;
use rpi_tui::{keybindings::keys, Keybindings, TuiAltScreen};

pub(crate) fn install(tui: &TuiAltScreen, bindings: Arc<Keybindings>) {
    tui.set_scroll_to_end_indicator(Some(Arc::new(move || {
        let colors = rpi_tui::theme().colors;
        format!(
            "{}{}{}\x1b[39m\x1b[49m",
            colors.selection.to_bg(),
            colors.text.to_fg(),
            label(&bindings)
        )
    })));
}

fn label(bindings: &Keybindings) -> String {
    let shortcut = bindings
        .get_keys(keys::BOTTOM)
        .iter()
        .map(|key| key.display())
        .collect::<Vec<_>>()
        .join("/");
    if shortcut.is_empty() {
        " ↓ Jump to latest message ".to_owned()
    } else {
        format!(" ↓ Jump to latest message · {shortcut} ")
    }
}

pub(crate) fn handle_key(tui: &TuiAltScreen, bindings: &Keybindings, key: &KeyEvent) -> bool {
    if bindings.matches(key, keys::BOTTOM) {
        tui.scroll_to_bottom();
    } else if bindings.matches(key, keys::TOP) {
        tui.scroll_to_top();
    } else if bindings.matches(key, keys::PAGE_UP) || bindings.matches(key, keys::PAGE_DOWN) {
        let height = tui
            .get_primary_scroll_view()
            .map_or(1, |scroll| scroll.viewport_height());
        let page = transcript_page_size(height);
        tui.scroll_by(if bindings.matches(key, keys::PAGE_UP) {
            -page
        } else {
            page
        });
    } else {
        return false;
    }
    true
}

pub(crate) fn transcript_page_size(viewport_height: usize) -> i32 {
    viewport_height
        .saturating_sub(4)
        .max(1)
        .min(i32::MAX as usize) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use rpi_tui::KeyCombo;

    #[test]
    fn indicator_hint_uses_the_actual_binding_and_hides_disabled_shortcuts() {
        let mut bindings = Keybindings::new();
        assert_eq!(label(&bindings), " ↓ Jump to latest message · End ");
        bindings.set(
            keys::BOTTOM,
            vec![KeyCombo::new(KeyCode::End, KeyModifiers::CONTROL)],
        );
        assert_eq!(label(&bindings), " ↓ Jump to latest message · Ctrl+End ");
        bindings.set(keys::BOTTOM, Vec::new());
        assert_eq!(label(&bindings), " ↓ Jump to latest message ");
    }
}
