//! Context-aware command-line completion.
//!
//! The input bar is a real editor, so it can offer rich completions
//! without asking the shell: executable names from `PATH`, shell
//! builtins, paths relative to the terminal's working directory, git branch
//! names, and commands from shell history.
//!
//! Everything here is pure: the caller passes the buffer, cursor, working
//! directory and history, and gets back a replacement range plus candidates.
//! That keeps the logic testable without a `Window`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Candidates returned for one completion request.
const MAX_ITEMS: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Directory,
    File,
    Branch,
    History,
}

impl CompletionKind {
    pub fn label(self) -> &'static str {
        match self {
            CompletionKind::Command => "command",
            CompletionKind::Directory => "dir",
            CompletionKind::File => "file",
            CompletionKind::Branch => "branch",
            CompletionKind::History => "history",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    /// Text that replaces the token.
    pub value: String,
    /// Text shown in the menu.
    pub display: String,
    pub detail: String,
    pub kind: CompletionKind,
    /// A directory completion: keep the menu open and complete deeper.
    pub is_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionResult {
    /// Byte offset of the token start in the buffer.
    pub start: usize,
    /// Byte offset of the token end (the cursor).
    pub end: usize,
    pub items: Vec<CompletionItem>,
}

/// Compute completions for the token under the cursor.
pub fn complete(
    buffer: &str,
    cursor: usize,
    cwd: &Path,
    history: &[String],
) -> Option<CompletionResult> {
    let cursor = cursor.min(buffer.len());
    let line_start = buffer[..cursor].rfind('\n').map_or(0, |idx| idx + 1);
    let line = &buffer[line_start..cursor];
    let (token_start, in_quotes) = token_start(line);
    let token = &line[token_start..];
    let before = &line[..token_start];
    let start = line_start + token_start;

    // A flag is never completed; the shell/TUI owns those.
    if token.starts_with('-') || token.starts_with("--") {
        return None;
    }

    let command = command_name(line);
    let command_position = is_command_position(before);
    let words: Vec<&str> = before.split_whitespace().collect();
    let last_word = words.last().copied().unwrap_or("");

    let mut items: Vec<CompletionItem> = Vec::new();

    if command_position {
        items.extend(command_candidates(token, history));
    } else if command == "git" && words.len() == 1 {
        items.extend(git_subcommand_candidates(token, in_quotes));
    } else if command == "git" && is_branch_subcommand(last_word) {
        items.extend(branch_candidates(cwd, token, in_quotes));
    }

    if !command_position || token.contains('/') || token.starts_with('.') || token.starts_with('~')
    {
        items.extend(path_candidates(cwd, token, in_quotes));
    }

    dedupe(&mut items);
    items.truncate(MAX_ITEMS);
    if items.is_empty() {
        None
    } else {
        Some(CompletionResult {
            start,
            end: cursor,
            items,
        })
    }
}

/// Most recent history entry that extends `value`.
///
/// Only single-line entries are suggested as ghost text: a multi-line ghost
/// would reflow the input bar for something the user did not ask to insert.
pub fn history_suggestion(value: &str, cursor: usize, history: &[String]) -> Option<String> {
    if value.is_empty() || cursor != utf16_len(value) || value.trim().is_empty() {
        return None;
    }
    history.iter().rev().find_map(|entry| {
        if entry.len() > value.len() && entry.starts_with(value) && !entry.contains('\n') {
            Some(entry[value.len()..].to_string())
        } else {
            None
        }
    })
}

pub fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// Byte offset for a UTF-16 offset, clamped to the end of the string.
pub fn utf16_to_byte(value: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    for (byte, ch) in value.char_indices() {
        if utf16 >= offset {
            return byte;
        }
        utf16 += ch.len_utf16();
    }
    value.len()
}

/// UTF-16 offset for a byte offset.
pub fn byte_to_utf16(value: &str, offset: usize) -> usize {
    utf16_len(&value[..offset.min(value.len())])
}

/// Start of the token being completed, relative to the line.
fn token_start(line: &str) -> (usize, bool) {
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => {
                in_single = !in_single;
                if in_single {
                    start = idx + 1;
                }
            }
            '"' if !in_single => {
                in_double = !in_double;
                if in_double {
                    start = idx + 1;
                }
            }
            c if !in_single
                && !in_double
                && (c.is_whitespace() || matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')')) =>
            {
                start = idx + c.len_utf8();
            }
            _ => {}
        }
    }

    (start, in_single || in_double)
}

/// `true` when a new command begins at this position (`|`, `;`, `sudo`, …).
fn is_command_position(before: &str) -> bool {
    let trimmed = before.trim_end();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.ends_with('|')
        || trimmed.ends_with('&')
        || trimmed.ends_with(';')
        || trimmed.ends_with('(')
        || trimmed.ends_with('{')
    {
        return true;
    }
    matches!(
        trimmed.split_whitespace().last().unwrap_or(""),
        "sudo"
            | "doas"
            | "env"
            | "time"
            | "command"
            | "exec"
            | "nohup"
            | "builtin"
            | "nice"
            | "xargs"
            | "then"
            | "else"
            | "do"
            | "if"
            | "while"
            | "until"
            | "!"
    )
}

/// First word of the line, without a directory prefix or extension quirk.
fn command_name(line: &str) -> &str {
    let first = line.split_whitespace().next().unwrap_or("");
    Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first)
}

const GIT_SUBCOMMANDS: &[&str] = &[
    "add",
    "am",
    "archive",
    "bisect",
    "blame",
    "branch",
    "bundle",
    "checkout",
    "cherry-pick",
    "clean",
    "clone",
    "commit",
    "config",
    "describe",
    "diff",
    "fetch",
    "format-patch",
    "fsck",
    "gc",
    "grep",
    "init",
    "log",
    "merge",
    "mv",
    "pull",
    "push",
    "rebase",
    "reflog",
    "remote",
    "reset",
    "restore",
    "revert",
    "rm",
    "shortlog",
    "show",
    "stash",
    "status",
    "submodule",
    "switch",
    "tag",
    "worktree",
];

fn git_subcommand_candidates(token: &str, in_quotes: bool) -> Vec<CompletionItem> {
    GIT_SUBCOMMANDS
        .iter()
        .filter(|subcommand| subcommand.starts_with(token))
        .map(|subcommand| CompletionItem {
            value: if in_quotes {
                (*subcommand).to_string()
            } else {
                escape_path(subcommand)
            },
            display: (*subcommand).to_string(),
            detail: "git".to_string(),
            kind: CompletionKind::Command,
            is_dir: false,
        })
        .collect()
}

fn is_branch_subcommand(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "checkout" | "switch" | "branch" | "merge" | "rebase" | "diff" | "log" | "show" | "reset"
    )
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn command_candidates(token: &str, history: &[String]) -> Vec<CompletionItem> {
    let mut seen = HashSet::new();
    let mut items = Vec::new();

    if token.contains('/') {
        return items;
    }

    for name in commands().iter() {
        if name.starts_with(token) && seen.insert(name.clone()) {
            items.push(CompletionItem {
                value: name.clone(),
                display: name.clone(),
                detail: "command".to_string(),
                kind: CompletionKind::Command,
                is_dir: false,
            });
        }
    }

    for builtin in SHELL_BUILTINS {
        if builtin.starts_with(token) && seen.insert((*builtin).to_string()) {
            items.push(CompletionItem {
                value: (*builtin).to_string(),
                display: (*builtin).to_string(),
                detail: "builtin".to_string(),
                kind: CompletionKind::Command,
                is_dir: false,
            });
        }
    }

    for entry in history.iter().rev() {
        let candidate = entry.split_whitespace().next().unwrap_or("");
        if candidate.is_empty()
            || !candidate.starts_with(token)
            || !seen.insert(candidate.to_string())
        {
            continue;
        }
        items.push(CompletionItem {
            value: candidate.to_string(),
            display: candidate.to_string(),
            detail: "history".to_string(),
            kind: CompletionKind::History,
            is_dir: false,
        });
    }

    items.sort_by(|a, b| a.display.cmp(&b.display));
    items
}

const SHELL_BUILTINS: &[&str] = &[
    "alias", "bg", "break", "builtin", "case", "cd", "command", "continue", "declare", "dirs",
    "disown", "do", "done", "echo", "else", "esac", "eval", "exec", "exit", "export", "false",
    "fc", "fg", "fi", "for", "function", "getopts", "hash", "help", "history", "if", "jobs",
    "kill", "let", "local", "logout", "popd", "printf", "pushd", "pwd", "read", "readonly",
    "return", "set", "shift", "source", "test", "then", "time", "times", "trap", "true", "type",
    "typeset", "ulimit", "umask", "unalias", "unset", "until", "wait", "which", "while",
];

/// Build the `PATH` index ahead of the first Tab so completion never has to
/// scan the filesystem on a keystroke.
pub fn prewarm() {
    let _ = commands();
}

/// `PATH` executables, scanned once per process.
fn commands() -> &'static Arc<Vec<String>> {
    static COMMANDS: OnceLock<Arc<Vec<String>>> = OnceLock::new();
    COMMANDS.get_or_init(|| {
        let mut names = std::collections::BTreeSet::new();
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.is_empty() || name.starts_with('.') {
                        continue;
                    }
                    if is_executable(&entry.path()) {
                        names.insert(name);
                    }
                }
            }
        }
        Arc::new(names.into_iter().collect())
    })
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn path_candidates(cwd: &Path, token: &str, in_quotes: bool) -> Vec<CompletionItem> {
    let (dir_part, prefix) = if token == "~" {
        ("~/", "")
    } else {
        match token.rfind('/') {
            Some(idx) => (&token[..=idx], &token[idx + 1..]),
            None => ("", token),
        }
    };

    // The token may carry backslash escapes ("my\ dir"); the filesystem wants
    // the unescaped form while the replacement keeps what the user typed.
    let fs_dir = unescape(dir_part);
    let match_prefix = unescape(prefix);

    let base = if let Some(rest) = fs_dir.strip_prefix("~/") {
        match home_dir() {
            Some(home) => home.join(rest),
            None => return Vec::new(),
        }
    } else if fs_dir == "~" || fs_dir == "~/" {
        match home_dir() {
            Some(home) => home,
            None => return Vec::new(),
        }
    } else if fs_dir.starts_with('/') {
        PathBuf::from(&fs_dir)
    } else {
        cwd.join(&fs_dir)
    };

    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };

    let show_hidden = match_prefix.starts_with('.');
    let mut items = Vec::new();
    let mut insensitive = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        if name.starts_with('.') && !show_hidden {
            continue;
        }

        let is_dir = entry.path().is_dir();
        let display = if is_dir {
            format!("{name}/")
        } else {
            name.clone()
        };
        let escaped_name = if in_quotes {
            name.clone()
        } else {
            escape_path(&name)
        };
        let mut value = format!("{dir_part}{escaped_name}");
        if is_dir {
            value.push('/');
        }
        let item = CompletionItem {
            value,
            display,
            detail: if is_dir {
                "dir".to_string()
            } else {
                "file".to_string()
            },
            kind: if is_dir {
                CompletionKind::Directory
            } else {
                CompletionKind::File
            },
            is_dir,
        };

        if name.starts_with(&match_prefix) {
            items.push(item);
        } else if name
            .to_lowercase()
            .starts_with(&match_prefix.to_lowercase())
        {
            insensitive.push(item);
        }
    }

    if items.is_empty() {
        items = insensitive;
    }

    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()))
    });
    items.truncate(MAX_ITEMS);
    items
}

fn escape_path(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(
            ch,
            ' ' | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '&'
                | ';'
                | '|'
                | '<'
                | '>'
                | '$'
                | '\''
                | '"'
                | '`'
                | '*'
                | '?'
                | '!'
                | '#'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}

// ---------------------------------------------------------------------------
// Git branches
// ---------------------------------------------------------------------------

fn branch_candidates(cwd: &Path, token: &str, in_quotes: bool) -> Vec<CompletionItem> {
    git_branches(cwd)
        .into_iter()
        .filter(|branch| branch.starts_with(token))
        .map(|branch| {
            let value = if in_quotes {
                branch.clone()
            } else {
                escape_path(&branch)
            };
            CompletionItem {
                value,
                display: branch.clone(),
                detail: "branch".to_string(),
                kind: CompletionKind::Branch,
                is_dir: false,
            }
        })
        .take(MAX_ITEMS)
        .collect()
}

fn git_branches(cwd: &Path) -> Vec<String> {
    let Some(git_dir) = find_git_dir(cwd) else {
        return Vec::new();
    };

    let mut branches = Vec::new();
    collect_refs(&git_dir.join("refs/heads"), "", &mut branches);

    if let Ok(packed) = std::fs::read_to_string(git_dir.join("packed-refs")) {
        for line in packed.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((_, reference)) = line.split_once(' ') {
                if let Some(name) = reference.strip_prefix("refs/heads/") {
                    branches.push(name.to_string());
                }
            }
        }
    }

    branches.sort();
    branches.dedup();
    branches
}

fn collect_refs(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect_refs(&path, &relative, out);
        } else {
            out.push(relative);
        }
    }
}

fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        let candidate = current.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let contents = std::fs::read_to_string(&candidate).ok()?;
            let target = contents
                .lines()
                .find_map(|line| line.strip_prefix("gitdir:"))?;
            let target = target.trim();
            return Some(if Path::new(target).is_absolute() {
                PathBuf::from(target)
            } else {
                current.join(target)
            });
        }
        current = current.parent()?;
    }
}

fn dedupe(items: &mut Vec<CompletionItem>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.value.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kathamo-completion-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn history_suggestion_needs_cursor_at_end_and_a_longer_match() {
        let history = vec!["cargo build".to_string(), "git status".to_string()];
        assert_eq!(
            history_suggestion("git st", 6, &history),
            Some("atus".to_string())
        );
        assert_eq!(history_suggestion("git st", 0, &history), None);
        assert_eq!(history_suggestion("git status", 10, &history), None);
        assert_eq!(history_suggestion("", 0, &history), None);
    }

    #[test]
    fn completes_paths_in_the_working_directory() {
        let dir = temp_dir("paths");
        std::fs::create_dir_all(dir.join("source")).unwrap();
        std::fs::write(dir.join("source/main.rs"), "").unwrap();
        std::fs::write(dir.join("readme.md"), "").unwrap();

        let result = complete("cd sou", 6, &dir, &[]).expect("completions");
        assert_eq!(result.start, 3);
        assert_eq!(result.end, 6);
        assert_eq!(result.items[0].value, "source/");
        assert!(result.items[0].is_dir);

        let deeper = complete("cd source/ma", 12, &dir, &[]).expect("deeper completions");
        assert_eq!(deeper.items[0].value, "source/main.rs");
        assert!(!deeper.items[0].is_dir);
    }

    #[test]
    fn completes_paths_for_leading_dot_and_home() {
        let dir = temp_dir("dots");
        std::fs::write(dir.join(".env"), "").unwrap();

        let result = complete("cat .", 5, &dir, &[]).expect("dot completion");
        assert_eq!(result.items[0].value, ".env");
    }

    #[test]
    fn completes_commands_in_command_position_only() {
        let dir = temp_dir("commands");
        let result = complete("zzz", 3, &dir, &[]);
        assert!(result.is_none());

        let result = complete("", 0, &dir, &[]).expect("empty command position lists PATH");
        assert!(!result.items.is_empty());
        assert!(
            result
                .items
                .iter()
                .all(|item| item.kind != CompletionKind::File)
        );
    }

    #[test]
    fn completes_paths_with_escaped_spaces() {
        let dir = temp_dir("spaces");
        std::fs::create_dir_all(dir.join("my dir")).unwrap();
        std::fs::write(dir.join("my dir/file.txt"), "").unwrap();

        let result = complete("cd my\\ d", 8, &dir, &[]).expect("escaped path completion");
        assert_eq!(result.items[0].value, "my\\ dir/");
        assert!(result.items[0].is_dir);
    }

    #[test]
    fn completes_git_subcommands() {
        let dir = temp_dir("git-subcommands");
        let result = complete("git che", 7, &dir, &[]).expect("git subcommand completions");
        let values: Vec<&str> = result
            .items
            .iter()
            .map(|item| item.value.as_str())
            .collect();
        assert!(values.contains(&"checkout"));
        assert!(values.contains(&"cherry-pick"));
        assert!(!values.contains(&"status"));
    }

    #[test]
    fn completes_git_branches() {
        let dir = temp_dir("branches");
        let git = dir.join(".git");
        std::fs::create_dir_all(git.join("refs/heads/feature")).unwrap();
        std::fs::write(git.join("refs/heads/main"), "ref").unwrap();
        std::fs::write(git.join("refs/heads/feature/login"), "ref").unwrap();
        std::fs::write(
            git.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled\nabc refs/heads/packed\n",
        )
        .unwrap();

        let result = complete("git checkout ", 13, &dir, &[]).expect("branch completions");
        let values: Vec<&str> = result
            .items
            .iter()
            .map(|item| item.value.as_str())
            .collect();
        assert!(values.contains(&"main"));
        assert!(values.contains(&"feature/login"));
        assert!(values.contains(&"packed"));
    }

    #[test]
    fn token_scanning_respects_quotes() {
        assert_eq!(token_start("echo 'hello wor"), (6, true));
        assert_eq!(token_start("ls src/ma"), (3, false));
        assert_eq!(token_start("git add . && ls so"), (16, false));
    }

    #[test]
    fn utf16_conversions_are_char_safe() {
        let text = "echo héllo";
        let byte = utf16_to_byte(text, 7);
        assert_eq!(&text[..byte], "echo hé");
        assert_eq!(byte_to_utf16(text, byte), 7);
    }
}
