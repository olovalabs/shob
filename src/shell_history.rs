//! Shell history loading and merging.
//!
//! Kathamo's input bar is backed by the shell's own command history, not just the
//! commands typed since the app started: on first use the
//! common history files (zsh, bash, fish) are read once and cached for the
//! process, then merged with the in-session commands each time the input bar
//! needs a history list.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Upper bound on the number of shell entries kept in memory.
const MAX_ENTRIES: usize = 4_000;
/// A single command longer than this is almost certainly binary junk or a
/// pathological file; skip it rather than keeping it in history.
const MAX_COMMAND_LEN: usize = 8_192;

/// Shell history shared by every pane in this process.
///
/// Loading is done once: a fresh pane only pays an `Arc` clone.
pub fn cached() -> Arc<Vec<String>> {
    static CACHE: OnceLock<Arc<Vec<String>>> = OnceLock::new();
    CACHE.get_or_init(|| Arc::new(load())).clone()
}

/// Merge on-disk shell history with the commands executed in this session.
///
/// The session commands come last because they are the most recent; adjacent
/// duplicates are collapsed so pressing Up never shows the same line twice.
pub fn merge(shell: &[String], session: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(shell.len() + session.len());
    for command in shell.iter().chain(session.iter()) {
        let command = command.trim();
        if command.is_empty() || command.len() > MAX_COMMAND_LEN {
            continue;
        }
        if out.last().map(|last: &String| last == command) == Some(true) {
            continue;
        }
        out.push(command.to_string());
    }
    out
}

/// History entries matching `query` (case-insensitive substring), newest first.
pub fn search<'a>(history: &'a [String], query: &str) -> Vec<&'a String> {
    let needle = query.trim().to_lowercase();
    let mut out: Vec<&String> = history
        .iter()
        .rev()
        .filter(|command| needle.is_empty() || command.to_lowercase().contains(&needle))
        .collect();
    out.truncate(100);
    out
}

fn load() -> Vec<String> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };

    let mut files = Vec::new();
    match std::env::var_os("HISTFILE").map(PathBuf::from) {
        Some(explicit) => files.push(explicit),
        None => {
            files.push(home.join(".zsh_history"));
            files.push(home.join(".bash_history"));
            files.push(fish_history_path(&home));
        }
    }

    let mut entries = Vec::new();
    for file in files {
        match file.file_name().and_then(|name| name.to_str()) {
            Some(name) if name.contains("fish") => entries.extend(read_fish(&file)),
            Some(name) if name.contains("zsh") => entries.extend(read_zsh(&file)),
            _ => entries.extend(read_bash(&file)),
        }
    }

    dedupe_recent(entries)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}

fn fish_history_path(home: &Path) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("fish/fish_history")
}

/// Bash stores one command per line, with optional `#<timestamp>` headers.
fn read_bash(path: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Zsh's extended history prefixes commands with `: <start>:<elapsed>;`.
/// Multi-line commands are stored with a trailing backslash continuation.
fn read_zsh(path: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut current = String::new();
    for raw in contents.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let line = match line.strip_prefix(": ") {
            Some(rest) => rest
                .split_once(';')
                .map(|(_, command)| command)
                .unwrap_or(rest),
            None => line,
        };

        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);

        if current.ends_with('\\') {
            current.pop();
            continue;
        }
        if !current.trim().is_empty() {
            out.push(std::mem::take(&mut current));
        } else {
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Fish writes a small YAML-ish list: `- cmd: <command>` followed by metadata
/// lines. Newlines are escaped as `\n`.
fn read_fish(path: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| line.strip_prefix("- cmd: "))
        .map(|command| {
            command
                .replace("\\n", "\n")
                .replace("\\\\", "\\")
                .trim_end()
                .to_string()
        })
        .filter(|command| !command.is_empty())
        .collect()
}

/// Keep the most recent occurrence of each distinct command, preserving order.
fn dedupe_recent(entries: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for command in entries.into_iter().rev() {
        let command = command.trim();
        if command.is_empty() || command.len() > MAX_COMMAND_LEN {
            continue;
        }
        if seen.insert(command.to_string()) {
            out.push(command.to_string());
        }
    }
    out.reverse();
    if out.len() > MAX_ENTRIES {
        out.drain(..out.len() - MAX_ENTRIES);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("kathamo-history-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).expect("write history fixture");
        path
    }

    #[test]
    fn parses_zsh_extended_history() {
        let path = write_temp(
            "zsh",
            ": 1700000000:0;git status\n: 1700000001:0;cargo test\n",
        );
        assert_eq!(read_zsh(&path), vec!["git status", "cargo test"]);
    }

    #[test]
    fn joins_zsh_continuation_lines() {
        let path = write_temp(
            "zsh-multiline",
            ": 1:0;for i in 1 2 3; do \\\necho $i \\\ndone\n",
        );
        assert_eq!(read_zsh(&path), vec!["for i in 1 2 3; do \necho $i \ndone"]);
    }

    #[test]
    fn parses_bash_history_and_skips_timestamps() {
        let path = write_temp("bash", "#1700000000\nls -la\necho hi\n");
        assert_eq!(read_bash(&path), vec!["ls -la", "echo hi"]);
    }

    #[test]
    fn parses_fish_history() {
        let path = write_temp(
            "fish",
            "- cmd: git log\n  when: 1700000000\n- cmd: echo a\\nb\n",
        );
        assert_eq!(read_fish(&path), vec!["git log", "echo a\nb"]);
    }

    #[test]
    fn merge_collapses_adjacent_duplicates_and_keeps_order() {
        let shell = vec!["ls".to_string(), "pwd".to_string()];
        let session = vec!["pwd".to_string(), "echo hi".to_string()];
        assert_eq!(merge(&shell, &session), vec!["ls", "pwd", "echo hi"]);
    }

    #[test]
    fn search_is_newest_first_and_case_insensitive() {
        let history = vec![
            "git status".to_string(),
            "cargo test".to_string(),
            "Git commit".to_string(),
        ];
        assert_eq!(search(&history, "git"), vec!["Git commit", "git status"]);
    }
}
