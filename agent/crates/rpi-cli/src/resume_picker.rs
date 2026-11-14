//! Startup session picker used by `-r` / `--resume`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use rpi_tui::{
    Container, ProcessTerminal, SelectItem, SelectList, SelectListLayoutOptions, Spacer, Text,
    TuiAltScreen, TUI,
};

/// Select a project session before the main harness is built.
///
/// Returns `Ok(None)` when the user cancels with Esc or Ctrl+C.
pub async fn select(cwd: &Path) -> Result<Option<String>, String> {
    select_in_dir(cwd, &crate::session::default_session_dir(cwd)).await
}

pub(crate) async fn select_in_dir(cwd: &Path, dir: &Path) -> Result<Option<String>, String> {
    let cwd_text = cwd.to_string_lossy().into_owned();
    let metadata = crate::session::list_session_metadata_in_dir(&cwd_text, dir)
        .await
        .map_err(|e| e.to_string())?;
    if metadata.is_empty() {
        return Err(format!("no sessions found in {}", dir.display()));
    }

    // Each summary opens + reads a (bounded) slice of the session file. That is
    // blocking file I/O over every saved session, so keep it off the async
    // runtime the TUI will later run on.
    let metas_for_summary = metadata.clone();
    let summaries = tokio::task::spawn_blocking(move || {
        metas_for_summary
            .iter()
            .map(|meta| crate::session::summarize_session_file(Path::new(&meta.path)))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("session summary task failed: {e}"))?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let mut items: Vec<SelectItem> = Vec::new();
    for (meta, summary) in metadata.iter().zip(summaries.iter()) {
        // A header-only session has nothing to restore; offering it is how a
        // resume lands on an empty transcript. Skip them so every listed row is
        // resumable.
        if summary.empty {
            continue;
        }
        let file_name = Path::new(&meta.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&meta.id);
        let label = crate::session::session_display_label(summary);
        let short_id = crate::session::short_session_id(&meta.id);
        let description = format!(
            "{} · {} · {}",
            format_modified_age(now_ms, meta.modified_at),
            crate::session::format_session_bytes(summary.bytes),
            short_id,
        );
        items.push(
            SelectItem::new(&meta.id, &label)
                .with_description(&description)
                .with_search_text(&format!("{label} {short_id} {file_name}")),
        );
    }
    if items.is_empty() {
        return Err(format!(
            "no resumable sessions with messages in {}",
            dir.display()
        ));
    }

    // A wide primary column so the name/preview label is not clipped to the
    // 32-column default (which showed only the timestamp prefix of the file
    // name and made sessions indistinguishable).
    let mut list = SelectList::new(items, 12);
    list.set_layout(SelectListLayoutOptions {
        min_primary_column_width: Some(56),
        max_primary_column_width: Some(72),
        truncate_primary: None,
    });
    let list = Arc::new(list);
    let search = Arc::new(Text::new("  Filter: ", 0, 0));
    let root = Arc::new(Container::new());
    root.add_child(Arc::new(Text::new("Resume session", 1, 1)));
    root.add_child(Arc::new(Text::new(
        "  Type to filter, Enter to resume, Esc to cancel",
        0,
        0,
    )));
    root.add_child(Arc::new(Spacer::new(1)));
    root.add_child(search.clone());
    root.add_child(Arc::new(Spacer::new(1)));
    root.add_child(list.clone());

    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        false,
        None,
    ));
    tui.set_layout_root(Some(root));
    // Short-lived selector, but the same native-pi frame throttle applies: its
    // keystroke bursts coalesce instead of repainting per keypress.
    tui.start_render_scheduler();
    tui.start_readerless();

    let input_tui = tui.clone();
    let input_list = list.clone();
    let input_search = search.clone();
    let task = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
        let mut query = String::new();
        loop {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => {}
                // Watchdog: a resize event can be missed, and this picker owns
                // no periodic tick of its own.
                Ok(false) => {
                    input_tui.ensure_size();
                    continue;
                }
                Err(e) => return Err(format!("could not read terminal input: {e}")),
            }
            let event = crossterm::event::read()
                .map_err(|e| format!("could not read terminal input: {e}"))?;
            match event {
                Event::Resize(..) => {
                    input_tui.note_resize();
                }
                Event::Mouse(mouse) => {
                    let code = match mouse.kind {
                        MouseEventKind::ScrollUp => Some(KeyCode::Up),
                        MouseEventKind::ScrollDown => Some(KeyCode::Down),
                        _ => None,
                    };
                    if let Some(code) = code {
                        input_list.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
                        input_tui.request_render(false);
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        return Ok(None);
                    }
                    match key.code {
                        KeyCode::Enter => {
                            return Ok(input_list.get_selected_item().map(|item| item.value));
                        }
                        KeyCode::Esc if query.is_empty() => return Ok(None),
                        KeyCode::Esc => {
                            query.clear();
                            input_list.set_filter("");
                        }
                        KeyCode::Backspace => {
                            query.pop();
                            input_list.set_filter(&query);
                        }
                        KeyCode::Char(ch)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            query.push(ch);
                            input_list.set_filter(&query);
                        }
                        _ => input_list.handle_key(key),
                    }
                    input_search.set_text(format!("  Filter: {query}"));
                    input_tui.request_render(false);
                }
                _ => {}
            }
        }
    });

    let result = task
        .await
        .map_err(|e| format!("session picker task failed: {e}"));
    tui.stop(Default::default());
    result?
}

fn format_modified_age(now_ms: i64, modified_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(modified_ms).max(0) / 1000;
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_age_is_compact_and_stable() {
        let now = 10 * 86_400_000;
        assert_eq!(format_modified_age(now, now - 20_000), "just now");
        assert_eq!(format_modified_age(now, now - 5 * 60_000), "5m ago");
        assert_eq!(format_modified_age(now, now - 3 * 3_600_000), "3h ago");
        assert_eq!(format_modified_age(now, now - 2 * 86_400_000), "2d ago");
    }

    #[test]
    fn label_falls_back_from_name_to_preview_to_marker() {
        use crate::session::{session_display_label, SessionSummary};
        let named = SessionSummary {
            name: Some("fix resume picker".into()),
            preview: Some("a different prompt".into()),
            ..Default::default()
        };
        assert_eq!(session_display_label(&named), "fix resume picker");

        let preview = SessionSummary {
            name: Some("   ".into()),
            preview: Some("读取下当前 rpi -r 命令".into()),
            ..Default::default()
        };
        assert_eq!(session_display_label(&preview), "读取下当前 rpi -r 命令");

        let bare = SessionSummary::default();
        assert_eq!(session_display_label(&bare), "(no messages)");
    }

    #[test]
    fn byte_and_id_formatting_is_compact() {
        use crate::session::{format_session_bytes, short_session_id};
        assert_eq!(format_session_bytes(512), "512B");
        assert_eq!(format_session_bytes(34 * 1024), "34KB");
        assert_eq!(format_session_bytes(1_258_291), "1.2MB");
        assert_eq!(
            short_session_id("01a0d919-6742-70db-a206-9f5ed4cf665a"),
            "01a0d919"
        );
        assert_eq!(short_session_id("abc"), "abc");
    }
}
