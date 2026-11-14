//! P3 — JSONL event journal.
//!
//! Opt-in via `RPI_EVENT_LOG=1`. When enabled, every extension **event-handler
//! invocation** (the observe fan-out in [`dispatch_to_handlers`] and the
//! veto-aware lifecycle dispatch in
//! [`dispatch_lifecycle_event`](crate::dispatch_lifecycle_event)) is recorded as
//! one JSON line, so a user can answer "why didn't my extension run?" and audit
//! handler behavior after the fact.
//!
//! Off by default (zero disk writes / zero overhead when disabled — the global
//! logger resolves to a no-op). One file, append-only:
//! `$RPI_EVENT_LOG_PATH`, else `<root>/logs/events.jsonl`, where `<root>` is
//! `$RPI_CODING_AGENT_DIR`'s parent (i.e. `~/.rpi`) or `~/.rpi`.
//!
//! [`dispatch_to_handlers`]: crate::translate::dispatch_to_handlers

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// One recorded handler invocation.
#[derive(Serialize)]
struct EventLogEntry<'a> {
    /// Unix epoch milliseconds (no chrono dep; a consumer can format it).
    ts_ms: u64,
    /// The event tag (`Debug` form, e.g. `BeforeTuiStart`, `MessageEnd`).
    event: String,
    /// The owning extension's display name.
    plugin: &'a str,
    /// `Continue` | `Error` | `Abort` | `Timeout` | `Panic` | `JoinFailed`.
    result: &'a str,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// A best-effort append-only JSONL sink. `writer` is `None` when logging is
/// disabled (env gate off, or the file could not be opened).
pub struct EventLogger {
    writer: Option<Mutex<BufWriter<std::fs::File>>>,
}

impl EventLogger {
    /// A disabled logger (no file).
    pub fn disabled() -> Self {
        Self { writer: None }
    }

    /// Open (create + append) an explicit path. Used by tests and by
    /// [`from_env`](Self::from_env).
    pub fn open(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => Self {
                writer: Some(Mutex::new(BufWriter::new(file))),
            },
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not open event log");
                Self { writer: None }
            }
        }
    }

    /// Resolve the log from the environment (`RPI_EVENT_LOG=1`). Disabled when
    /// the gate is off or no path can be resolved.
    pub fn from_env() -> Self {
        if std::env::var("RPI_EVENT_LOG").ok().as_deref() != Some("1") {
            return Self::disabled();
        }
        match log_path() {
            Some(path) => Self::open(path),
            None => Self::disabled(),
        }
    }

    /// Whether this logger will actually write.
    pub fn enabled(&self) -> bool {
        self.writer.is_some()
    }

    /// Append one entry (best-effort; write errors are swallowed — logging must
    /// never break a dispatch).
    fn record(
        &self,
        event: &str,
        plugin: &str,
        result: &'static str,
        duration_ms: u64,
        detail: Option<String>,
    ) {
        let Some(writer) = &self.writer else {
            return;
        };
        let entry = EventLogEntry {
            ts_ms: now_ms(),
            event: event.to_string(),
            plugin,
            result,
            duration_ms,
            detail,
        };
        // A poisoned lock (a panic mid-write) must not kill logging.
        let mut guard = match writer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = serde_json::to_writer(&mut *guard, &entry);
        let _ = guard.write_all(b"\n");
        let _ = guard.flush();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Resolve the event-log path (`$RPI_EVENT_LOG_PATH`, else
/// `<root>/logs/events.jsonl`, where `<root>` is `$RPI_CODING_AGENT_DIR`'s
/// parent or `~/.rpi`). Public so `rpi events tail` can find the same file the
/// logger writes.
pub fn event_log_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RPI_EVENT_LOG_PATH") {
        return Some(PathBuf::from(path));
    }
    let root = if let Some(agent) = std::env::var_os("RPI_CODING_AGENT_DIR") {
        // The override is the *agent* dir (`~/.rpi/agent`); logs live in the
        // parent (`~/.rpi/logs`). A parentless override falls back to itself.
        let agent = PathBuf::from(agent);
        agent.parent().map(PathBuf::from).unwrap_or(agent)
    } else {
        dirs::home_dir()?.join(".rpi")
    };
    Some(root.join("logs").join("events.jsonl"))
}

/// Internal alias for [`event_log_path`].
fn log_path() -> Option<PathBuf> {
    event_log_path()
}

/// The process-global event logger slot. Initialized lazily from the
/// environment on first use; a test may replace it via
/// `set_event_logger_for_test`.
fn global() -> &'static RwLock<Option<EventLogger>> {
    static GLOBAL: OnceLock<RwLock<Option<EventLogger>>> = OnceLock::new();
    GLOBAL.get_or_init(|| RwLock::new(Some(EventLogger::from_env())))
}

/// Record one handler invocation on the global logger. No-op when logging is
/// disabled (the common case).
pub fn log_handler_invocation(
    tag: rpi_plugin_sdk::EventTag,
    plugin: &str,
    result: &'static str,
    duration_ms: u64,
    detail: Option<String>,
) {
    let guard = match global().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(logger) = guard.as_ref() {
        if logger.enabled() {
            logger.record(&format!("{tag:?}"), plugin, result, duration_ms, detail);
        }
    }
}

/// Test-only: install an explicit logger (replacing the env-derived default).
/// Callers must restore [`EventLogger::disabled`] when done to avoid leaking the
/// sink into other tests.
#[cfg(test)]
pub(crate) fn set_event_logger_for_test(logger: EventLogger) {
    let mut guard = match global().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(logger);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_jsonl_lines_and_is_noop_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");

        // Disabled logger writes nothing.
        let disabled = EventLogger::disabled();
        assert!(!disabled.enabled());
        disabled.record("X", "p", "Continue", 0, None);

        // An explicit logger appends one JSON line per record.
        let logger = EventLogger::open(path.clone());
        assert!(logger.enabled());
        logger.record(
            "BeforeTuiStart",
            "rpc-ext",
            "Abort",
            42,
            Some("connect failed".into()),
        );
        logger.record("MessageEnd", "logger-ext", "Continue", 1, None);

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "BeforeTuiStart");
        assert_eq!(first["plugin"], "rpc-ext");
        assert_eq!(first["result"], "Abort");
        assert_eq!(first["duration_ms"], 42);
        assert_eq!(first["detail"], "connect failed");
        assert!(first["ts_ms"].is_u64());
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["event"], "MessageEnd");
        // Omitted `detail` must not appear.
        assert!(second.get("detail").is_none());
    }
}
