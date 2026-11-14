use gpui_component::input::LineChange;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub struct FileContent {
    pub text: String,
    pub deleted: bool,
    pub changes: Vec<LineChange>,
}

pub fn read(root: PathBuf, path: String) -> Result<FileContent, String> {
    let full_path = root.join(&path);
    let (bytes, deleted) = match std::fs::File::open(&full_path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("Could not read file: {e}"))?;
            (bytes, false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let object = format!("HEAD:{path}");
            let size = Command::new("git")
                .current_dir(&root)
                .args(["cat-file", "-s", &object])
                .output()
                .map_err(|e| format!("Could not read deleted file: {e}"))?;
            if !size.status.success() {
                return Err("File no longer exists in the working tree or HEAD.".into());
            }
            let size: u64 = String::from_utf8_lossy(&size.stdout)
                .trim()
                .parse()
                .map_err(|_| "Could not determine file size.")?;
            if size > MAX_BYTES {
                return Err("File is larger than the 4 MiB preview limit.".into());
            }
            let output = Command::new("git")
                .current_dir(&root)
                .args(["show", "--no-ext-diff", &object])
                .output()
                .map_err(|e| format!("Could not read deleted file: {e}"))?;
            if !output.status.success() {
                return Err("Could not read the file from HEAD.".into());
            }
            (output.stdout, true)
        }
        Err(error) => return Err(format!("Could not open file: {error}")),
    };
    if bytes.len() as u64 > MAX_BYTES {
        return Err("File is larger than the 4 MiB preview limit.".into());
    }
    if bytes.contains(&0) {
        return Err("Binary files cannot be shown in the text editor.".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "File is not valid UTF-8 text.")?;
    let changes = line_changes(&root, &path, &text, deleted);
    Ok(FileContent {
        text,
        deleted,
        changes,
    })
}

// The preview is a snapshot, so compute markers alongside the file read rather
// than on the UI thread. A missing repository or Git failure leaves no markers.
fn line_changes(root: &Path, path: &str, text: &str, deleted: bool) -> Vec<LineChange> {
    let line_count = text.lines().count().max(1);
    let output = match Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    let repo = PathBuf::from(
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_string(),
    );
    let full_path = root.join(path);
    let Ok(relative) = full_path.strip_prefix(&repo) else {
        return Vec::new();
    };
    let Some(relative) = relative.to_str() else {
        return Vec::new();
    };
    if deleted {
        return vec![
            LineChange {
                deleted: true,
                ..Default::default()
            };
            line_count
        ];
    }
    let run = |args: &[&str]| {
        Command::new("git")
            .current_dir(&repo)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .arg("--literal-pathspecs")
            .args(args)
            .output()
    };
    let has_head = run(&["rev-parse", "--verify", "HEAD"]).is_ok_and(|out| out.status.success());
    let tracked =
        run(&["ls-files", "--error-unmatch", "--", relative]).is_ok_and(|out| out.status.success());
    if !has_head || !tracked {
        return vec![
            LineChange {
                added: true,
                ..Default::default()
            };
            line_count
        ];
    }
    let Ok(diff) = run(&[
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-renames",
        "--unified=0",
        "HEAD",
        "--",
        relative,
    ]) else {
        return Vec::new();
    };
    if !diff.status.success() {
        return Vec::new();
    }
    parse_diff_markers(&String::from_utf8_lossy(&diff.stdout), line_count)
}

fn parse_diff_markers(patch: &str, line_count: usize) -> Vec<LineChange> {
    let mut changes = vec![LineChange::default(); line_count];
    for header in patch.lines().filter(|line| line.starts_with("@@ -")) {
        let mut parts = header.split_whitespace();
        let (_, Some(old), Some(new)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let parse = |part: &str| -> Option<(usize, usize)> {
            let mut numbers = part.get(1..)?.split(',');
            let start = numbers.next()?.parse().ok()?;
            let count = numbers.next().map_or(Some(1), |n| n.parse().ok())?;
            Some((start, count))
        };
        let (Some((_, old_count)), Some((start, new_count))) = (parse(old), parse(new)) else {
            continue;
        };
        let first = start.saturating_sub(1);
        for row in first..first.saturating_add(new_count).min(line_count) {
            if old_count == 0 {
                changes[row].added = true;
            } else {
                changes[row].modified = true;
            }
        }
        if old_count > new_count && line_count > 0 {
            // A zero-length new range is positioned after `start`; otherwise
            // mark the line after the replacement, clamped at the file end.
            let boundary = if new_count == 0 {
                start
            } else {
                first.saturating_add(new_count)
            };
            changes[boundary.min(line_count - 1)].deleted = true;
        }
    }
    changes
}

/// Resolve local paths and file URLs without routing network URLs into the editor.
pub fn local_link_path(target: &str, base: &Path) -> Option<PathBuf> {
    if target.is_empty() || target.starts_with('#') {
        return None;
    }
    let (target, _, _) = terminal::hyperlink::parse_path_with_position(target);
    let url = match url::Url::parse(&target) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase) => url::Url::from_directory_path(base)
            .ok()?
            .join(&target)
            .ok()?,
        Err(_) => return None,
    };
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

pub fn language(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "makefile" | "gnumakefile" | "bsdmakefile" => return "make".into(),
        "cmakelists.txt" => return "cmake".into(),
        "gemfile" | "rakefile" | "guardfile" | "podfile" | "vagrantfile" => return "ruby".into(),
        ".bashrc" | ".bash_profile" | ".profile" | ".zshrc" | ".zprofile" => return "bash".into(),
        _ => {}
    }
    match Path::new(&lower)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
    {
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "h" => "c",
        "hh" | "hpp" | "hxx" | "cxx" | "cc" | "c++" => "cpp",
        "sh" | "bash" | "zsh" => "bash",
        "cs" => "csharp",
        "py" | "pyi" | "pyw" => "python",
        "rb" | "gemspec" => "ruby",
        "rs" => "rust",
        "md" | "markdown" | "mdown" => "markdown",
        "yml" => "yaml",
        "ex" | "exs" => "elixir",
        "graphql" | "gql" => "graphql",
        "patch" => "diff",
        "htm" => "html",
        "jsonc" => "json",
        extension => extension,
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    #[test]
    fn registered_highlight_queries_compile() {
        use gpui_component::highlighter::LanguageRegistry;
        let registry = LanguageRegistry::singleton();
        for name in registry.languages() {
            let config = registry.language(&name).unwrap();
            if name == "text" {
                continue;
            }
            assert!(
                !config.highlights.is_empty(),
                "{name} has no highlight query"
            );
            tree_sitter::Query::new(&config.language, &config.highlights)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
        }
    }

    #[test]
    fn preview_highlighters_produce_styles_for_code() {
        use gpui_component::highlighter::{HighlightTheme, SyntaxHighlighter};
        for (path, source) in [
            ("main.rs", "fn main() { let n = 1; }"),
            ("main.py", "def main():\n    return 42\n"),
            ("app.tsx", "const App = () => <div>Hello</div>;"),
            ("main.go", "package main\nfunc main() {}"),
            ("main.cs", "class Example { int value = 42; }"),
            ("main.swift", "let greeting = \"hello\""),
            ("query.graphql", "query { viewer { name } } # comment"),
            (
                "schema.proto",
                "syntax = \"proto3\"; message User { string name = 1; }",
            ),
        ] {
            let text = gpui_component::Rope::from_str(source);
            let mut highlighter = SyntaxHighlighter::new(&language(path));
            highlighter.update(None, &text);
            assert!(
                !highlighter
                    .styles(&(0..source.len()), &HighlightTheme::default_dark())
                    .is_empty(),
                "{path} produced no styles"
            );
        }
    }

    #[test]
    fn detects_languages_from_common_filenames_and_extensions() {
        for (path, expected) in [
            ("src/main.rs", "rust"),
            ("src/App.tsx", "tsx"),
            ("src/App.jsx", "javascript"),
            ("script.py", "python"),
            ("Makefile", "make"),
            ("CMakeLists.txt", "cmake"),
            ("Gemfile", "ruby"),
            (".bashrc", "bash"),
            ("settings.YML", "yaml"),
            ("change.patch", "diff"),
            ("unknown.filetype", "filetype"),
        ] {
            assert_eq!(language(path), expected, "{path}");
        }
    }

    #[test]
    fn resolves_local_links_without_intercepting_web_urls() {
        let base = Path::new("/tmp/my-project");
        assert_eq!(
            local_link_path("src/App.svelte", base),
            Some(base.join("src/App.svelte"))
        );
        assert_eq!(
            local_link_path("file:///tmp/hello%20world.rs", base),
            Some(PathBuf::from("/tmp/hello world.rs"))
        );
        assert_eq!(
            local_link_path("src/main.rs:12:3", base),
            Some(base.join("src/main.rs"))
        );
        assert_eq!(local_link_path("https://example.com/a.md", base), None);
        assert_eq!(local_link_path("#section", base), None);
    }

    #[test]
    fn marks_staged_unstaged_added_and_deleted_lines_against_head() {
        let dir = std::env::temp_dir().join(format!(
            "kathamo-markers-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("sample.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        std::fs::write(dir.join("sample.txt"), "one\nchanged\nthree\nfour\n").unwrap();
        git(&["add", "sample.txt"]);
        std::fs::write(dir.join("sample.txt"), "one\nchanged\nthree\nfour\nnew\n").unwrap();
        let file = read(dir.clone(), "sample.txt".into()).unwrap();
        assert!(file.changes[1].modified);
        assert!(file.changes[4].added);
        assert_eq!(file.changes[0], LineChange::default());
        std::fs::write(dir.join("new.txt"), "new\nfile\n").unwrap();
        assert!(
            read(dir.clone(), "new.txt".into())
                .unwrap()
                .changes
                .iter()
                .all(|line| line.added)
        );
        std::fs::write(dir.join("sample.txt"), "one\nthree\nfour\n").unwrap();
        assert!(read(dir.clone(), "sample.txt".into()).unwrap().changes[1].deleted);
        std::fs::remove_file(dir.join("sample.txt")).unwrap();
        assert!(
            read(dir.clone(), "sample.txt".into())
                .unwrap()
                .changes
                .iter()
                .all(|line| line.deleted)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_text_rejects_binary_and_loads_deleted_from_head() {
        let dir = std::env::temp_dir().join(format!(
            "kathamo-file-preview-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("file.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("binary.bin"), [0, 1, 2]).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let live = read(dir.clone(), "file.rs".into()).unwrap();
        assert_eq!(live.text, "fn main() {}\n");
        assert!(!live.deleted);
        assert!(
            read(dir.clone(), "binary.bin".into())
                .err()
                .unwrap()
                .contains("Binary")
        );
        std::fs::remove_file(dir.join("file.rs")).unwrap();
        let deleted = read(dir.clone(), "file.rs".into()).unwrap();
        assert_eq!(deleted.text, "fn main() {}\n");
        assert!(deleted.deleted);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
