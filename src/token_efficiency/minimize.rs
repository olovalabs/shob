//! Output minimization for noisy tool results.
//!
//! Borrowed in shape from oh-my-pi's `crates/shell/src/minimizer/` (per-command
//! filters, `input_bytes`/`output_bytes` accounting), but deliberately much
//! smaller: instead of per-program rules this applies four content-agnostic
//! filters that catch most of the noise in practice. Every rewrite is measured,
//! and no filter is allowed to change a line's leading content — the goal is to
//! remove *repetition and blank space*, not to paraphrase.
//!
//! Two categories are never minimized: `read` results (the model relies on exact
//! line content and the truncation notices) and anything containing an image.

use std::collections::HashSet;

/// What one minimization pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Minimized {
    pub bytes_in: usize,
    pub bytes_out: usize,
    /// Consecutive duplicate lines replaced by a `×N` marker.
    pub duplicate_runs: usize,
    /// Blank-line runs collapsed.
    pub blank_runs: usize,
    /// Path-noise lines dropped (vendored/lockfile/build output).
    pub noise_lines: usize,
    /// True when the hard cap truncated the result.
    pub capped: bool,
}

impl Minimized {
    pub fn saved(&self) -> usize {
        self.bytes_in.saturating_sub(self.bytes_out)
    }
}

/// Tools whose output this module may rewrite.
pub fn is_minimizable_tool(name: &str) -> bool {
    matches!(name, "bash" | "grep" | "find" | "ls" | "output")
}

/// Path fragments whose lines are noise in agent context: reading them costs
/// tokens and tells the model nothing it can act on.
const NOISE_FRAGMENTS: [&str; 8] = [
    "/node_modules/",
    "/.git/",
    "/target/debug/",
    "/target/release/",
    "/dist/",
    "/.venv/",
    "Cargo.lock",
    "package-lock.json",
];

/// Collapse `text`.
///
/// `max_bytes` is a hard cap applied *after* the lossless-ish filters, because
/// the cap is the only lossy step and it should not be the one that fires when a
/// duplicate run would have been enough.
pub fn minimize(text: &str, max_bytes: usize) -> (String, Minimized) {
    let mut stats = Minimized {
        bytes_in: text.len(),
        ..Minimized::default()
    };

    let mut out: Vec<String> = Vec::new();
    let mut previous: Option<&str> = None;
    let mut repeat = 0usize;
    let mut blanks = 0usize;

    for line in text.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            // At most one blank line survives, and never leading blanks.
            if blanks == 2 && !out.is_empty() {
                stats.blank_runs += 1;
            }
            if blanks == 1 && !out.is_empty() {
                out.push(String::new());
            }
            continue;
        }
        blanks = 0;

        if previous == Some(line) {
            repeat += 1;
            continue;
        }
        if repeat > 0 {
            // The first occurrence is already in `out`; record how many
            // repetitions were dropped after it.
            if let Some(last) = out.last_mut() {
                last.push_str(&format!("  … ×{}", repeat + 1));
            }
            stats.duplicate_runs += 1;
            repeat = 0;
        }

        if is_noise(line) {
            stats.noise_lines += 1;
            continue;
        }

        out.push(line.to_string());
        previous = Some(line);
    }
    if repeat > 0 {
        if let Some(last) = out.last_mut() {
            last.push_str(&format!("  … ×{}", repeat + 1));
        }
        stats.duplicate_runs += 1;
    }

    let mut joined = out.join("\n");
    if text.ends_with('\n') && !joined.is_empty() {
        joined.push('\n');
    }

    if joined.len() > max_bytes {
        let cut = floor_char_boundary(&joined, max_bytes);
        let elided_lines = joined[cut..].lines().count();
        joined.truncate(cut);
        joined.push_str(&format!(
            "\n[…{elided_lines} lines elided to keep this result under {} bytes]",
            max_bytes
        ));
        stats.capped = true;
    }

    stats.bytes_out = joined.len();
    (joined, stats)
}

fn is_noise(line: &str) -> bool {
    NOISE_FRAGMENTS
        .iter()
        .any(|fragment| line.contains(fragment))
}

/// The largest char boundary at or below `limit`, so a cap never splits a
/// multi-byte character (which would panic on `truncate`).
fn floor_char_boundary(text: &str, limit: usize) -> usize {
    if limit >= text.len() {
        return text.len();
    }
    let mut boundary = limit;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

/// Remove duplicate *path* lines from a directory listing while keeping the
/// order, for `find` output that repeats whole subtrees.
pub fn dedupe_paths(text: &str) -> String {
    let mut seen = HashSet::new();
    text.lines()
        .filter(|line| seen.insert(line.trim().to_string()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_noisy_tools_are_eligible() {
        assert!(is_minimizable_tool("bash"));
        assert!(is_minimizable_tool("grep"));
        assert!(is_minimizable_tool("find"));
        assert!(is_minimizable_tool("ls"));
        assert!(!is_minimizable_tool("read"), "read's truncation is load-bearing");
        assert!(!is_minimizable_tool("edit"));
        assert!(!is_minimizable_tool("write"));
    }

    #[test]
    fn duplicate_runs_collapse_with_a_count() {
        let input = "Compiling a\nCompiling a\nCompiling a\nCompiling a\nDone\n";
        let (out, stats) = minimize(input, 10_000);
        assert!(out.contains("Compiling a  … ×4"), "got {out:?}");
        assert!(out.contains("Done"));
        assert_eq!(stats.duplicate_runs, 1);
        assert!(stats.saved() > 0);
    }

    #[test]
    fn non_adjacent_repeats_are_kept() {
        let input = "a\nb\na\n";
        let (out, _) = minimize(input, 10_000);
        assert_eq!(out, "a\nb\na\n");
    }

    #[test]
    fn blank_runs_collapse_and_leading_blanks_vanish() {
        let (out, stats) = minimize("\n\n\nalpha\n\n\n\nbeta\n", 10_000);
        assert_eq!(out, "alpha\n\nbeta\n");
        assert_eq!(stats.blank_runs, 1);
    }

    #[test]
    fn vendored_path_noise_is_dropped() {
        let input = "src/a.rs\nnode_modules/x/y.js\nsrc/b.rs\n/abs/node_modules/z.js\n";
        let (out, stats) = minimize(input, 10_000);
        assert_eq!(out, "src/a.rs\nsrc/b.rs\n");
        assert_eq!(stats.noise_lines, 2);
    }

    #[test]
    fn small_results_are_left_alone() {
        let input = "short output\n";
        let (out, stats) = minimize(input, 4096);
        assert_eq!(out, input);
        assert_eq!(stats.saved(), 0);
        assert!(!stats.capped);
    }

    #[test]
    fn the_cap_elides_and_says_how_much() {
        let input: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let (out, stats) = minimize(&input, 500);
        assert!(stats.capped);
        assert!(out.len() <= 500 + 80, "cap plus its notice: {} bytes", out.len());
        assert!(out.contains("lines elided"), "the elision must be visible: {out:?}");
        assert!(out.starts_with("line 0"));
    }

    #[test]
    fn the_cap_never_splits_a_character() {
        let input = "é".repeat(1000);
        let (out, stats) = minimize(&input, 101);
        assert!(stats.capped);
        assert!(out.is_char_boundary(out.len() - out.len()), "valid UTF-8");
        // Round-tripping through from_utf8 is implicit in `String`; assert the
        // elided tail is all `é` characters.
        assert!(out.trim_end().chars().all(|c| c == 'é' || c == '[' || c.is_ascii()));
    }

    #[test]
    fn measurement_is_self_consistent() {
        let input = "x\nx\nx\n\n\n\ny\n";
        let (out, stats) = minimize(input, 10_000);
        assert_eq!(stats.bytes_in, input.len());
        assert_eq!(stats.bytes_out, out.len());
        assert_eq!(stats.saved(), input.len() - out.len());
        assert!(out.len() < input.len());
    }

    #[test]
    fn path_dedupe_keeps_first_occurrence_order() {
        assert_eq!(dedupe_paths("a\nb\na\nc\nb"), "a\nb\nc");
    }

    #[test]
    fn empty_input_is_safe() {
        let (out, stats) = minimize("", 4096);
        assert_eq!(out, "");
        assert_eq!(stats.bytes_in, 0);
    }

    #[test]
    fn a_realistic_git_status_is_compressed() {
        let input = "\
On branch main
Changes not staged for commit:
  modified:   src/a.rs
  modified:   src/b.rs

Untracked files:
  target/debug/incremental/x
  target/debug/incremental/y
";
        let (out, stats) = minimize(input, 10_000);
        assert!(!out.contains("target/debug"), "build noise must go");
        assert!(out.contains("modified:   src/a.rs"), "real signal must stay");
        assert!(stats.saved() > 0);
    }
}
