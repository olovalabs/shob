//! Conservative, bounded formatting of an already bounded shell capture.
//! No command parsing, status inference, or reads of the unbounded persisted log.

use std::sync::LazyLock;

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Shell output is unchanged unless compact formatting is explicitly requested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShellOutputMode {
    /// Return the original sanitized output (the default).
    #[default]
    Raw,
    /// Collapse recognized Cargo, pytest, Go and JS passing/progress lines;
    /// retain diagnostics and save the complete sanitized output to a file.
    Compact,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShellOutputMetrics {
    pub execution_ms: f64,
    pub result_format_ms: f64,
    pub raw_bytes: usize,
    pub displayed_bytes: usize,
    pub progress_updates: usize,
    /// raw, compacted, partial, or unchanged (compact was not smaller).
    pub compact_status: &'static str,
}

fn progress_line(line: &str) -> bool {
    // Anchored grammars deliberately exclude failure, skipped and arbitrary
    // runner output. Final summaries remain verbatim; only passing/progress
    // records disappear. Compile these once, not once per tool invocation.
    static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
            r"^test [A-Za-z0-9_:<>]+ \.\.\. ok$",
            r"^(?:   Compiling|    Checking) [A-Za-z0-9_-]+ v[0-9][^\s]*(?: \([^\r\n]+\))?$",
            r"^    (?:Updating crates\.io index|Downloading crates \.\.\.)$",
            r"^tests?/[^\s]+\.py \.+\s+\[\s*(?:[0-9]{1,2}|100)%\]$",
            r"^--- PASS: [A-Za-z0-9_/]+ \([0-9]+\.[0-9]+s\)$",
            r"^ok\s+[^\s]+\s+[0-9]+\.[0-9]+s(?:\s+coverage: [0-9]+\.[0-9]+% of statements)?$",
            r"^PASS [^\s]+\.(?:test|spec)\.[cm]?[jt]sx?(?: \([0-9]+\.[0-9]+ s\))?$",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("static progress grammar"))
        .collect()
    });
    PATTERNS.iter().any(|pattern| pattern.is_match(line))
}

fn summary_or_header(line: &str) -> bool {
    line.strip_prefix("running ")
        .and_then(|s| s.strip_suffix(" tests").or_else(|| s.strip_suffix(" test")))
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        || line.starts_with("test result: ")
        || line.starts_with("Test Suites: ")
        || line.starts_with("Tests: ")
        || line.starts_with("Snapshots: ")
        || line.starts_with("Time: ")
        || line.starts_with("    Finished ")
        || line.starts_with("     Running ")
        || line.starts_with("   Doc-tests ")
        || line.starts_with("=== RUN   ")
        || line == "PASS"
}

fn diagnostic_line(line: &str) -> bool {
    let line = line.trim_start();
    [
        "error",
        "warning",
        "Error",
        "Warning",
        "FAIL",
        "panic",
        "thread '",
        "AssertionError",
        "assertion",
        "E   ",
        "--> ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

/// Returns a strictly smaller candidate, including its marker and recovery
/// path. A truncated capture is *never* treated as a complete runner stream.
/// Its existing head/tail notices and path are retained, and only adjacent
/// identical diagnostic lines can be grouped.
pub(crate) fn compact_output(raw: &str, path: &str, partial: bool) -> Option<String> {
    let mut output = String::with_capacity(raw.len());
    let mut lines = raw.split_inclusive('\n').peekable();
    let mut collapsed = 0usize;
    let mut grouped = 0usize;
    let mut diagnostic_block = false;
    let mut diagnostics_seen = false;
    while let Some(line) = lines.next() {
        let text = line.strip_suffix('\n').unwrap_or(line);
        let diagnostic = diagnostic_line(text);
        diagnostics_seen |= diagnostic;
        if text.is_empty() {
            diagnostic_block = false;
        }
        if !partial && !diagnostic_block && !diagnostics_seen && progress_line(text) {
            collapsed += 1;
            continue;
        }
        // Unknown text can be a diagnostic block; never discard runner-looking
        // lines embedded in it. A blank line ends the block.
        if !text.is_empty() && !summary_or_header(text) {
            diagnostic_block = true;
        }
        if diagnostic {
            let mut count = 1usize;
            let mut terminated = line.ends_with('\n');
            while lines
                .peek()
                .is_some_and(|next| next.strip_suffix('\n').unwrap_or(next) == text)
            {
                terminated = lines.next().expect("peeked line").ends_with('\n');
                count += 1;
            }
            if count > 1 {
                let annotation = format!(" [repeated {count} times]");
                let replacement_len = text.len() + annotation.len() + usize::from(terminated);
                let original_len = line.len().saturating_mul(count) - usize::from(!terminated);
                if replacement_len < original_len {
                    output.push_str(text);
                    output.push_str(&annotation);
                    if terminated {
                        output.push('\n');
                    }
                    grouped += count - 1;
                    continue;
                }
                for _ in 1..count {
                    output.push_str(line);
                }
                output.push_str(text);
                if terminated {
                    output.push('\n');
                }
                continue;
            }
        }
        output.push_str(line);
    }
    if collapsed == 0 && grouped == 0 {
        return None;
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    if partial {
        // The raw retained view already contains its single full-output hint.
        output.push_str(
            "[Compact partial retained view; repeated diagnostics grouped; not a complete summary]",
        );
    } else {
        output.push_str(&format!("[Compact: {collapsed} passing/progress lines omitted; {grouped} repeated diagnostic lines grouped. Full output: {path}]"));
    }
    (output.len() < raw.len()).then_some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_unknown_diagnostics_locations_and_assertions() {
        let raw = format!("{}test result: FAILED. 80 passed; 1 failed\n\nCUSTOM diagnostic\n  --> src/lib.rs:42:9\nassertion left == right failed\ntest misleading ... ok\n", "test crate::passes ... ok\n".repeat(80));
        let compact = compact_output(&raw, "/tmp/log", false).unwrap();
        assert!(compact.contains("test result: FAILED. 80 passed; 1 failed"));
        assert!(compact.contains("CUSTOM diagnostic\n  --> src/lib.rs:42:9\nassertion left == right failed\ntest misleading ... ok\n"));
        assert!(compact.contains("80 passing/progress lines omitted"));
    }

    #[test]
    fn exact_adjacent_repetition_counts_and_boundaries() {
        let a = "error: a very long diagnostic describing the actual failure\n";
        let b = "error: a different diagnostic with another location: foo.rs:8\n";
        let raw = format!("{}{}\n{}", a.repeat(20), b.repeat(10), a.repeat(5));
        let compact = compact_output(&raw, "/tmp/log", false).unwrap();
        assert!(compact.contains("[repeated 20 times]"));
        assert!(compact.contains("[repeated 10 times]"));
        assert!(compact.contains("[repeated 5 times]"));
        assert!(compact.contains("32 repeated diagnostic lines grouped"));
        assert!(compact.len() < raw.len());
    }

    #[test]
    fn partial_view_never_summarizes_passing_lines() {
        let passes = "test crate::passes ... ok\n".repeat(50);
        assert!(compact_output(&passes, "/tmp/log", true).is_none());
        let raw = format!(
            "{}{}\n[Full output: /tmp/log]",
            passes,
            "error: long repeated diagnostic with a source.rs:45 location\n".repeat(30)
        );
        let compact = compact_output(&raw, "/tmp/log", true).unwrap();
        assert!(compact.starts_with(&passes));
        assert!(compact.contains("partial retained view"));
        assert_eq!(compact.matches("Full output:").count(), 1);
    }

    #[test]
    fn recognized_runner_formats_keep_summaries() {
        for line in [
            "   Compiling example v0.1.0 (/tmp/example)\n",
            "    Checking example v0.1.0 (/tmp/example)\n",
            "tests/test_example.py ........ [ 80%]\n",
            "--- PASS: TestExample (0.01s)\n",
            "ok  example/package 0.001s\n",
            "PASS src/example.test.ts\n",
        ] {
            let raw = format!("{}test result: ok. 80 passed\n", line.repeat(80));
            let compact = compact_output(&raw, "/tmp/log", false).unwrap();
            assert!(compact.contains("test result: ok. 80 passed"), "{line}");
            assert!(
                compact.contains("80 passing/progress lines omitted"),
                "{line}"
            );
        }
    }

    #[test]
    fn final_unterminated_diagnostic_is_counted() {
        let line = "error: repeated failure in some_file.rs:56:7";
        let raw = format!("{}{}", format!("{line}\n").repeat(19), line);
        let compact = compact_output(&raw, "/tmp/log", false).unwrap();
        assert!(compact.contains("[repeated 20 times]"));
        assert!(compact.contains("19 repeated diagnostic lines grouped"));
    }

    #[test]
    fn short_or_unknown_output_stays_raw() {
        assert!(compact_output("test a ... ok\n", "/long/path/to/log", false).is_none());
        assert!(compact_output(&"unrecognized output\n".repeat(100), "/tmp/log", false).is_none());
    }

    #[test]
    fn passing_looking_lines_inside_diagnostics_survive_blank_lines() {
        let diagnostic = "error: diagnostic block\n\n test passes ... ok\ntest passes ... ok\n";
        let raw = format!(
            "{}{}",
            "test example::passes ... ok\n".repeat(80),
            diagnostic
        );
        let compact = compact_output(&raw, "/tmp/log", false).unwrap();
        assert!(compact.contains(diagnostic));
        assert!(compact.contains("80 passing/progress lines omitted"));
    }
}
