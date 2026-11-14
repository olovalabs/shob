//! Env-gated key-event tracing for diagnosing terminal input problems
//! (remote/mobile clients, "Enter inserted a newline", paste-vs-typed Enter…).
//!
//! Enable with `RPI_DEBUG_KEYS=1` (appends to `<temp>/rpi-keys.log`) or
//! `RPI_DEBUG_KEYS=<path>` to pick the file. Every key the TUI dispatches is
//! appended as one line — wall clock, key code, modifiers, event kind, plus a
//! caller-supplied note (the paste-burst decision, for example):
//!
//! ```text
//! 2026-09-24T12:00:01.234Z code=Char('h') mods=NONE kind=Press
//! 2026-09-24T12:00:01.235Z code=Enter mods=NONE kind=Press gap_ms=1 more_queued=false -> newline (paste burst, Windows only)
//! ```
//!
//! What a remote client sends for Enter varies. A phone/RDP client that
//! injects a real key event shows up as `code=Enter mods=NONE` (and
//! `mods=SHIFT` when Shift is held), while one that encodes Enter as a bare LF
//! shows up as `code=Char('j') mods=CONTROL` — in raw mode crossterm decodes the
//! byte `0x0A` through its control-code table (`0x0A` is both LF and Ctrl+J).
//! On Windows the paste-burst decision also prints `more_queued=`, which is why
//! the probe ignores a key's own `Release` event.
//!
//! Tracing is off unless the env var is set: when disabled the cost is a single
//! `OnceLock` lookup per key.

use std::io::Write;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Resolved trace destination, or `None` when tracing is off.
fn target() -> Option<&'static std::path::PathBuf> {
    static TARGET: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    TARGET
        .get_or_init(|| {
            let value = std::env::var("RPI_DEBUG_KEYS").ok()?;
            let value = value.trim();
            if value.is_empty() || value == "0" {
                return None;
            }
            if value == "1" || value.eq_ignore_ascii_case("true") {
                Some(std::env::temp_dir().join("rpi-keys.log"))
            } else {
                Some(std::path::PathBuf::from(value))
            }
        })
        .as_ref()
}

/// Whether key tracing is enabled.
pub fn enabled() -> bool {
    target().is_some()
}

/// Path the trace is written to (for the startup banner / user hints).
pub fn path() -> Option<&'static std::path::Path> {
    target().map(std::path::PathBuf::as_path)
}

/// Append one key event, with an optional note describing what the TUI decided.
pub fn key(event: &KeyEvent, note: &str) {
    let Some(path) = target() else {
        return;
    };
    let mut line = format!(
        "{} code={} mods={} kind={:?}",
        timestamp(),
        code_name(event.code),
        modifiers_name(event.modifiers),
        event.kind
    );
    if !note.is_empty() {
        line.push(' ');
        line.push_str(note);
    }
    line.push('\n');
    append(path, &line);
}

/// Append a free-form note (startup banner, terminal info, …).
pub fn note(text: &str) {
    let Some(path) = target() else {
        return;
    };
    append(path, &format!("{} {text}\n", timestamp()));
}

/// Paste-boundary diagnostics, independent of the per-key/content trace.
/// Only caller-supplied counts and decisions are recorded, never clipboard text.
/// Enable with RPI_DEBUG_PASTE=1 or a log file path for one test session.
pub fn paste_note(note: impl FnOnce() -> String) {
    static TARGET: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let path = TARGET.get_or_init(|| {
        let value = std::env::var("RPI_DEBUG_PASTE").ok()?;
        let value = value.trim();
        let path = if value.is_empty() || value == "0" {
            return None;
        } else if value == "1" || value.eq_ignore_ascii_case("true") {
            std::env::temp_dir().join("rpi-paste.log")
        } else {
            std::path::PathBuf::from(value)
        };
        append(
            &path,
            &format!(
                "{} paste run pid={} version={}\n",
                timestamp(),
                std::process::id(),
                env!("CARGO_PKG_VERSION")
            ),
        );
        Some(path)
    });
    if let Some(path) = path {
        append(path, &format!("{} paste {}\n", timestamp(), note()));
    }
}

fn append(path: &std::path::Path, line: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// UTC timestamp, `2026-09-24T12:00:01.234Z`.
fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.as_millis();
    let secs = (millis / 1000) as i64;
    let sub = millis % 1000;
    let (year, month, day, hour, min, sec) = civil_from_unix(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{sub:03}Z")
}

/// Days-from-civil inverse (Howard Hinnant's algorithm) — avoids a date crate
/// dependency for a diagnostic timestamp.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m as u32,
        d as u32,
        (secs_of_day / 3600) as u32,
        ((secs_of_day % 3600) / 60) as u32,
        (secs_of_day % 60) as u32,
    )
}

/// Readable key code — control characters are spelled out so an LF-as-Ctrl+J
/// (the mobile/remote Enter case) is visible at a glance.
fn code_name(code: KeyCode) -> String {
    match code {
        KeyCode::Char(c) if c.is_control() => {
            format!("Char({c:?}) [U+{:04X}]", c as u32)
        }
        KeyCode::Char(c) => format!("Char({c:?})"),
        other => format!("{other:?}"),
    }
}

fn modifiers_name(modifiers: KeyModifiers) -> String {
    if modifiers.is_empty() {
        return "NONE".to_string();
    }
    let mut parts = Vec::new();
    for (flag, name) in [
        (KeyModifiers::CONTROL, "CONTROL"),
        (KeyModifiers::SHIFT, "SHIFT"),
        (KeyModifiers::ALT, "ALT"),
        (KeyModifiers::SUPER, "SUPER"),
        (KeyModifiers::HYPER, "HYPER"),
        (KeyModifiers::META, "META"),
    ] {
        if modifiers.contains(flag) {
            parts.push(name);
        }
    }
    parts.join("|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_matches_the_unix_epoch() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(1_700_000_000), (2023, 11, 14, 22, 13, 20));
    }

    #[test]
    fn control_characters_and_modifiers_are_spelled_out() {
        assert_eq!(code_name(KeyCode::Char('j')), "Char('j')");
        assert_eq!(code_name(KeyCode::Char('\n')), "Char('\\n') [U+000A]");
        assert_eq!(code_name(KeyCode::Enter), "Enter");
        assert_eq!(
            modifiers_name(KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            "CONTROL|SHIFT"
        );
        assert_eq!(modifiers_name(KeyModifiers::NONE), "NONE");
    }
}
