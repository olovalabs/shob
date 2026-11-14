use gpui::{HighlightStyle, SharedString};
use gpui_component::Rope;
use gpui_component::highlighter::{HighlightTheme, SyntaxHighlighter};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;
use unicode_width::UnicodeWidthChar as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileGroup {
    Tracked,
    Untracked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFile {
    pub group: FileGroup,
    pub path: String,
    pub additions: Option<usize>,
    pub deletions: Option<usize>,
    pub patch: SharedString,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectDiff {
    pub root: PathBuf,
    pub files: Vec<DiffFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSummary {
    pub files: usize,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: SharedString,
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
}

#[derive(Default)]
pub struct InlineDiff {
    pub hunks: Vec<Vec<DiffLine>>,
    pub truncated: bool,
}

impl InlineDiff {
    pub fn from_patch(path: &str, patch: &str, theme: &HighlightTheme) -> Self {
        const MAX_LINES: usize = 1500;
        const MAX_BYTES: usize = 512 * 1024;

        let mut diff = Self::default();
        let mut lines = 0;
        let mut bytes = 0;
        for line in patch.lines() {
            if line.starts_with("@@ ") {
                diff.hunks.push(Vec::new());
                continue;
            }
            let Some(hunk) = diff.hunks.last_mut() else {
                continue;
            };
            let (kind, text) = match line.as_bytes().first() {
                Some(b' ') => (DiffLineKind::Context, &line[1..]),
                Some(b'+') => (DiffLineKind::Added, &line[1..]),
                Some(b'-') => (DiffLineKind::Removed, &line[1..]),
                None => (DiffLineKind::Context, ""),
                _ => continue, // Includes Git's missing-final-newline notice.
            };
            if lines == MAX_LINES || bytes + text.len() > MAX_BYTES {
                diff.truncated = true;
                break;
            }
            lines += 1;
            bytes += text.len();
            hunk.push(DiffLine {
                kind,
                text: text.into(),
                highlights: Vec::new(),
            });
        }

        diff.hunks.retain(|hunk| !hunk.is_empty());
        if !diff.hunks.is_empty() {
            let language = crate::file_preview::language(path);
            for hunk in &mut diff.hunks {
                // Parse each side separately so deleted code cannot disrupt the new syntax tree.
                highlight_hunk(hunk, DiffLineKind::Added, &language, theme);
                highlight_hunk(hunk, DiffLineKind::Removed, &language, theme);
                for line in hunk {
                    expand_tabs(line);
                }
            }
        }
        diff
    }
}

fn expand_tabs(line: &mut DiffLine) {
    if !line.text.contains('\t') {
        return;
    }
    let mut text = String::new();
    let mut column = 0;
    let mut offsets = Vec::new();
    let mut extra_bytes = 0;
    for (index, ch) in line.text.char_indices() {
        if ch == '\t' {
            let width = 4 - column % 4;
            text.extend(std::iter::repeat_n(' ', width));
            column += width;
            extra_bytes += width - 1;
            offsets.push((index + 1, extra_bytes));
        } else {
            text.push(ch);
            column += ch.width().unwrap_or_default();
        }
    }
    let offset = |index: usize| {
        let prior = offsets.partition_point(|(end, _)| *end <= index);
        index + prior.checked_sub(1).map_or(0, |prior| offsets[prior].1)
    };
    for (range, _) in &mut line.highlights {
        *range = offset(range.start)..offset(range.end);
    }
    line.text = text.into();
}

fn highlight_hunk(
    lines: &mut [DiffLine],
    side: DiffLineKind,
    language: &str,
    theme: &HighlightTheme,
) {
    let mut source = String::new();
    let ranges: Vec<_> = lines
        .iter()
        .map(|line| {
            if line.kind != DiffLineKind::Context && line.kind != side {
                return None;
            }
            let start = source.len();
            source.push_str(&line.text);
            let range = start..source.len();
            source.push('\n');
            Some(range)
        })
        .collect();
    if source.is_empty() || !lines.iter().any(|line| line.kind == side) {
        return;
    }

    let mut highlighter = SyntaxHighlighter::new(language);
    highlighter.update(None, &Rope::from_str(&source));
    for (line, range) in lines.iter_mut().zip(ranges) {
        let Some(range) = range else { continue };
        // Context has the same text on both sides. Prefer the working tree's styles.
        if range.is_empty()
            || range.len() > 4096
            || (side == DiffLineKind::Removed && !line.highlights.is_empty())
        {
            continue;
        }
        line.highlights = highlighter
            .styles(&range, theme)
            .into_iter()
            .filter_map(|(style_range, style)| {
                let start = style_range.start - range.start;
                let end = style_range.end - range.start;
                (start < end
                    && line.text.is_char_boundary(start)
                    && line.text.is_char_boundary(end))
                .then_some((start..end, style))
            })
            .collect();
    }
}

pub fn summary(directory: PathBuf) -> Result<ChangeSummary, String> {
    let diff = load_impl(directory, false)?;
    Ok(ChangeSummary {
        files: diff.files.len(),
        additions: diff.files.iter().filter_map(|file| file.additions).sum(),
        deletions: diff.files.iter().filter_map(|file| file.deletions).sum(),
    })
}

fn git(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| format!("Could not run Git: {e}"))
}

fn checked(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let output = git(root, args)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn no_index(root: &Path, path: &str, numstat: bool) -> Result<std::process::Output, String> {
    let mut args = vec!["diff", "--no-index", "--no-ext-diff", "--no-textconv"];
    if numstat {
        args.extend(["--numstat", "-z"]);
    } else {
        args.extend(["--no-color", "--unified=3", "--"]);
    }
    if numstat {
        args.push("--");
    }
    args.extend(["/dev/null", path]);
    let out = git(root, &args)?;
    if matches!(out.status.code(), Some(0 | 1)) {
        Ok(out)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

fn counts(record: &[u8]) -> Option<(Option<usize>, Option<usize>, String)> {
    let mut fields = record.splitn(3, |b| *b == b'\t');
    let added = std::str::from_utf8(fields.next()?).ok()?.parse().ok();
    let deleted = std::str::from_utf8(fields.next()?).ok()?.parse().ok();
    let path = String::from_utf8_lossy(fields.next()?).into_owned();
    Some((added, deleted, path))
}

pub fn load(directory: PathBuf) -> Result<ProjectDiff, String> {
    load_impl(directory, true)
}

fn load_impl(directory: PathBuf, with_patches: bool) -> Result<ProjectDiff, String> {
    let root_output = checked(&directory, &["rev-parse", "--show-toplevel"])
        .map_err(|e| format!("Not a Git repository: {e}"))?;
    let root = PathBuf::from(
        String::from_utf8_lossy(&root_output.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string(),
    );
    let has_head = git(&root, &["rev-parse", "--verify", "HEAD"])?
        .status
        .success();
    let mut files = Vec::new();
    if has_head {
        let stats = checked(
            &root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--numstat",
                "-z",
                "HEAD",
                "--",
            ],
        )?;
        for record in stats.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            if let Some((additions, deletions, path)) = counts(record) {
                let patch = if with_patches {
                    let output = checked(
                        &root,
                        &[
                            "diff",
                            "--no-ext-diff",
                            "--no-textconv",
                            "--no-renames",
                            "--no-color",
                            "--unified=3",
                            "HEAD",
                            "--",
                            &path,
                        ],
                    )?;
                    SharedString::from(String::from_utf8_lossy(&output.stdout).into_owned())
                } else {
                    SharedString::default()
                };
                files.push(DiffFile {
                    group: FileGroup::Tracked,
                    path,
                    additions,
                    deletions,
                    patch,
                });
            }
        }
    }
    // In a new repository, compare every present tracked file to an empty tree.
    // Untracked files are included separately for both new and existing repos.
    let mut paths = Vec::new();
    if !has_head {
        let tracked = checked(&root, &["ls-files", "-z"])?;
        paths.extend(
            tracked
                .stdout
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| (String::from_utf8_lossy(p).into_owned(), FileGroup::Tracked)),
        );
    }
    let untracked = checked(&root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    paths.extend(
        untracked
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| {
                (
                    String::from_utf8_lossy(p).into_owned(),
                    FileGroup::Untracked,
                )
            }),
    );
    paths.sort_by(|a, b| a.0.cmp(&b.0));
    paths.dedup_by(|a, b| a.0 == b.0);
    for (path, group) in paths {
        if !root.join(&path).is_file() {
            continue;
        }
        let stat = no_index(&root, &path, true)?;
        let Some((additions, deletions, _)) =
            counts(stat.stdout.split(|b| *b == 0).next().unwrap_or_default())
        else {
            continue;
        };
        let patch = if with_patches {
            SharedString::from(
                String::from_utf8_lossy(&no_index(&root, &path, false)?.stdout).into_owned(),
            )
        } else {
            SharedString::default()
        };
        files.push(DiffFile {
            group,
            path,
            additions,
            deletions,
            patch,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ProjectDiff { root, files })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Repo(PathBuf);
    impl Repo {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "kathamo-diff-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            let repo = Self(path);
            repo.run(&["init", "-q"]);
            repo.run(&["config", "user.name", "Test"]);
            repo.run(&["config", "user.email", "test@example.com"]);
            repo
        }
        fn run(&self, args: &[&str]) {
            let result = git(&self.0, args).unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        fn write(&self, path: &str, text: &[u8]) {
            std::fs::write(self.0.join(path), text).unwrap();
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn combines_index_worktree_and_untracked_against_head() {
        let repo = Repo::new();
        repo.write("file with space.txt", b"one\ntwo\n");
        repo.run(&["add", "."]);
        repo.run(&["commit", "-qm", "base"]);
        repo.write("file with space.txt", b"one\nstaged\n");
        repo.run(&["add", "."]);
        repo.write("file with space.txt", b"one\nstaged\nworktree\n");
        repo.write("new.txt", b"new\nfile\n");
        let diff = load(repo.0.clone()).unwrap();
        assert_eq!(
            summary(repo.0.clone()).unwrap(),
            ChangeSummary {
                files: 2,
                additions: 4,
                deletions: 1
            }
        );
        assert_eq!(diff.files.len(), 2);
        let tracked = &diff.files[0];
        assert_eq!(tracked.path, "file with space.txt");
        assert_eq!(tracked.group, FileGroup::Tracked);
        assert_eq!(diff.files[1].group, FileGroup::Untracked);
        assert_eq!((tracked.additions, tracked.deletions), (Some(2), Some(1)));
        assert!(tracked.patch.contains("+worktree"));
        let inline = InlineDiff::from_patch(
            &tracked.path,
            &tracked.patch,
            &HighlightTheme::default_light(),
        );
        assert_eq!(
            inline.hunks[0]
                .iter()
                .map(|line| (line.kind, line.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (DiffLineKind::Context, "one"),
                (DiffLineKind::Removed, "two"),
                (DiffLineKind::Added, "staged"),
                (DiffLineKind::Added, "worktree"),
            ],
        );
        assert_eq!(
            (diff.files[1].additions, diff.files[1].deletions),
            (Some(2), Some(0))
        );
    }

    #[test]
    fn inline_diff_preserves_code_prefixes_and_highlights_each_side() {
        let patch = "diff --git a/example.rs b/example.rs\n--- a/example.rs\n+++ b/example.rs\n\
                     @@ -1,3 +1,3 @@\n fn example() {\n-\tlet value = \"旧\";\n+\tlet value = \"新\";\n }\n\
                     @@ -9 +9 @@\n-/// Old comment\n+/// New comment\n\\ No newline at end of file\n";
        let diff = InlineDiff::from_patch("example.rs", patch, &HighlightTheme::default_light());
        assert_eq!(diff.hunks.len(), 2);
        assert_eq!(diff.hunks[0].len(), 4);
        assert_eq!(diff.hunks[1].len(), 2);
        for line in &diff.hunks[0][1..3] {
            assert!(line.text.starts_with("    let value"));
            assert!(line.highlights.iter().any(|(range, style)| {
                style.color.is_some() && &line.text[range.clone()] == "let"
            }));
        }
        assert_eq!(diff.hunks[1][1].text, "/// New comment");
    }

    #[test]
    fn preserves_dotfiles_and_lockfiles_and_respects_gitignore() {
        let repo = Repo::new();
        repo.write(".gitignore", b"ignored.log\ntracked.lock\n");
        repo.write("tracked.lock", b"old\n");
        repo.run(&["add", ".gitignore"]);
        repo.run(&["add", "-f", "tracked.lock"]);
        repo.run(&["commit", "-qm", "base"]);
        repo.write(".gitignore", b"ignored.log\ntracked.lock\nmore\n");
        repo.write("tracked.lock", b"new\n");
        repo.write(".new-config", b"visible\n");
        repo.write("ignored.log", b"ignored\n");
        let diff = load(repo.0.clone()).unwrap();
        assert_eq!(
            diff.files
                .iter()
                .map(|file| (file.path.as_str(), file.group))
                .collect::<Vec<_>>(),
            vec![
                (".gitignore", FileGroup::Tracked),
                (".new-config", FileGroup::Untracked),
                ("tracked.lock", FileGroup::Tracked)
            ]
        );
    }

    #[test]
    fn shows_unborn_repository_and_binary_files() {
        let repo = Repo::new();
        repo.write("staged.txt", b"one\ntwo\n");
        repo.run(&["add", "staged.txt"]);
        repo.write("binary.dat", b"\0\x01\x02");
        let diff = load(repo.0.clone()).unwrap();
        assert_eq!(diff.files.len(), 2);
        assert_eq!(diff.files[0].group, FileGroup::Untracked);
        assert_eq!(diff.files[1].group, FileGroup::Tracked);
        assert_eq!(
            (diff.files[0].additions, diff.files[0].deletions),
            (None, None)
        );
        assert_eq!(
            (diff.files[1].additions, diff.files[1].deletions),
            (Some(2), Some(0))
        );
    }
}
