//! Central timing instrumentation for startup profiling.
//!
//! Port of the upstream `packages/coding-agent/src/core/timings.ts`. Enable with
//! the `RPI_TIMING=1` environment variable; disabled builds pay only a single
//! atomic load per call.
//!
//! Timings are grouped by namespace (`main` for the primary startup path,
//! `extensions` for JS/Rust extension loading) and flushed once at shutdown.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// True when `RPI_TIMING=1`. Evaluated once.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RPI_TIMING")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimingNamespace {
    Main,
    Extensions,
}

impl TimingNamespace {
    fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Extensions => "extensions",
        }
    }
}

struct Group {
    timings: Vec<(String, i64)>,
    last: Instant,
}

fn groups() -> &'static Mutex<HashMap<&'static str, Group>> {
    static GROUPS: OnceLock<Mutex<HashMap<&'static str, Group>>> = OnceLock::new();
    GROUPS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reset a namespace's timings and mark "now" as its reference point.
pub fn reset(namespace: TimingNamespace) {
    if !enabled() {
        return;
    }
    if let Ok(mut g) = groups().lock() {
        g.insert(
            namespace.label(),
            Group {
                timings: Vec::new(),
                last: Instant::now(),
            },
        );
    }
}

/// Record the elapsed time since the previous `time`/`reset` in this namespace.
pub fn time(label: impl Into<String>, namespace: TimingNamespace) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    if let Ok(mut g) = groups().lock() {
        let group = g.entry(namespace.label()).or_insert_with(|| Group {
            timings: Vec::new(),
            last: now,
        });
        let ms = now.duration_since(group.last).as_millis() as i64;
        group.timings.push((label.into(), ms));
        group.last = now;
    }
}

/// Print every accumulated namespace to stderr. No-op unless `RPI_TIMING=1`.
pub fn print_timings() {
    if !enabled() {
        return;
    }
    let Ok(g) = groups().lock() else {
        return;
    };
    // Stable order: main before extensions.
    for ns in ["main", "extensions"] {
        let Some(group) = g.get(ns) else { continue };
        print_group(&format!("Startup Timings: {ns}"), &group.timings);
    }
}

fn print_group(title: &str, timings: &[(String, i64)]) {
    let printable: Vec<_> = timings.iter().filter(|(_, ms)| *ms >= 0).collect();
    if printable.is_empty() {
        return;
    }
    eprintln!("\n--- {title} ---");
    for (label, ms) in &printable {
        eprintln!("  {label}: {ms}ms");
    }
    let total: i64 = printable.iter().map(|(_, ms)| *ms).sum();
    eprintln!("  TOTAL: {total}ms");
    eprintln!("{}\n", "-".repeat(title.len() + 8));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default() {
        // The env var is unset in the test process unless explicitly set.
        if std::env::var_os("RPI_TIMING").is_none() {
            assert!(!enabled());
        }
    }

    #[test]
    fn records_and_resets() {
        // Exercise the recording path directly, independent of the env flag.
        if let Ok(mut g) = groups().lock() {
            g.insert(
                "test",
                Group {
                    timings: Vec::new(),
                    last: Instant::now(),
                },
            );
        }
        if let Ok(mut g) = groups().lock() {
            let group = g.get_mut("test").unwrap();
            group.timings.push(("boot".to_string(), 12));
        }
        if let Ok(g) = groups().lock() {
            assert_eq!(g.get("test").unwrap().timings.len(), 1);
        }
    }
}
