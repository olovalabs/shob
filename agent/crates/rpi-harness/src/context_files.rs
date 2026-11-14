//! Project context-file discovery. Mirrors `packages/coding-agent/src/core/
//! resource-loader.ts` (`loadContextFileFromDir` + `loadProjectContextFiles`).
//!
//! Context files are per-directory instruction files (`AGENTS.md`/`CLAUDE.md`
//! family) pi auto-injects into the system prompt as a `<project_context>`
//! block. Unlike skills (which are a named registry) and prompt-templates
//! (which are on-demand `/expand` targets), context files are **always
//! injected** when present — they are the project's standing instructions.
//!
//! Discovery contract (verified against `resource-loader.ts:70-156`):
//! - Per directory, the **first match** of `["AGENTS.override.md", "AGENTS.md",
//!   "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"]` is loaded (AGENTS family wins over
//!   CLAUDE — `resource-loader.ts:71`).
//! - `loadProjectContextFiles` walks from the global `agentDir` first, then
//!   ancestor-walks `cwd → root`, **unshift**-ing each dir's match so the
//!   deepest (closest to cwd) file is concatenated **last** (`:118-156`).
//! - Each canonical path appears at most once (`seenPaths`, `:126`).
//!
//! v1 divergences (documented): the TS worktree *shadowed* context-file
//! deduplication (`findShadowedContextFile`, `:100-116`) is deferred — it
//! suppresses a linked worktree's duplicate of the main repo's file and is a
//! git-layout edge case rpi's resource discovery does not yet need.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rpi_tools::env::{ExecutionEnv, FileKind};
use tokio_util::sync::CancellationToken;

/// Per-directory candidate filenames, **in priority order**. First existing
/// match wins. Mirrors `loadContextFileFromDir`'s `candidates` array
/// (`resource-loader.ts:71`) — the AGENTS family takes precedence over the
/// CLAUDE family, and `.override.md` wins over the plain `AGENTS.md`.
pub const CONTEXT_FILE_CANDIDATES: &[&str] = &[
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// One discovered context file: its absolute path + raw content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// Load the first context file present in `dir`, or `None`. Mirrors
/// `loadContextFileFromDir` (`resource-loader.ts:70-89`): iterate the candidate
/// list in order, return the first that exists and is a regular file. A
/// candidate that exists but is a directory is skipped (`continue`); a read
/// failure returns `None` (the TS path logs a warning — here the caller's
/// `Diagnostics` channel is owned by the project layer, so this loader is
/// quiet on read errors, matching the "missing dir is skipped" posture of the
/// sibling `load_skills`).
pub async fn load_context_file_from_dir(
    env: &Arc<dyn ExecutionEnv>,
    dir: impl AsRef<Path>,
) -> Option<ContextFile> {
    let dir_str = dir.as_ref().to_string_lossy().to_string();
    let cancel = CancellationToken::new();
    for name in CONTEXT_FILE_CANDIDATES {
        let candidate = Path::new(&dir_str).join(name);
        let path_str = candidate.to_string_lossy().to_string();
        let info = match env.file_info(&path_str, Some(&cancel)).await {
            Ok(info) => info,
            Err(e) if e.code == rpi_tools::FileErrorCode::NotFound => continue,
            Err(_) => continue,
        };
        // Skip directories masquerading as a candidate name.
        if info.kind != FileKind::File {
            continue;
        }
        let content = match env.read_text_file(&path_str, Some(&cancel)).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        return Some(ContextFile {
            path: candidate,
            content,
        });
    }
    None
}

/// Discover the full context-file set for a project. Mirrors
/// `loadProjectContextFiles` (`resource-loader.ts:118-156`):
///
/// 1. The global context file under `agent_dir` (the user's standing
///    instructions) is loaded first and appears at the head of the list.
/// 2. Then walk `cwd → filesystem root`, loading each ancestor dir's first
///    match and **unshift**-ing it in front of the accumulated ancestor list,
///    so the deepest (closest to `cwd`) file ends up **last** in the output.
/// 3. Each canonical *path* is included at most once (`seen`).
///
/// The result is concatenated in order into the `<project_context>` block by
/// [`format_project_context`], yielding global-then-shallow-then-deep order,
/// matching pi.
pub async fn load_project_context_files(
    env: &Arc<dyn ExecutionEnv>,
    cwd: &Path,
    agent_dir: &Path,
) -> Vec<ContextFile> {
    let mut context_files: Vec<ContextFile> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 1. Global context (agentDir).
    if let Some(global) = load_context_file_from_dir(env, agent_dir).await {
        let key = canonical(&global.path);
        seen.insert(key.clone());
        context_files.push(global);
    }

    // 2. Ancestor walk cwd → root, unshift-ing so cwd's file is last.
    let mut ancestors: Vec<ContextFile> = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        if let Some(file) = load_context_file_from_dir(env, &current).await {
            let key = canonical(&file.path);
            if !seen.contains(&key) {
                seen.insert(key);
                // `unshift`: prepend so earlier (shallower) discoveries get
                // pushed down as we walk further toward root — the final
                // ancestor list is root-first, cwd-last.
                ancestors.insert(0, file);
            }
        }
        let parent = match current.parent() {
            Some(p) if p != current => p.to_path_buf(),
            _ => break,
        };
        current = parent;
    }

    context_files.extend(ancestors);
    context_files
}

/// Render the context-file list as a `<project_context>` block for the system
/// prompt. Mirrors `createCodingSystemPrompt`'s context section
/// (`system-prompt.ts:54-60,144-151`): a wrapper tag, a one-line lead, then one
/// `<project_instructions path="...">` child per file. Returns `""` when there
/// are no context files (so a caller can string-interpolate the result without
/// leaving an empty block).
pub fn format_project_context(files: &[ContextFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("\n\n<project_context>\n\n");
    out.push_str("Project-specific instructions and guidelines:\n\n");
    for file in files {
        let path = escape_path(&file.path.to_string_lossy());
        out.push_str(&format!(
            "<project_instructions path=\"{path}\">\n{}\n</project_instructions>\n\n",
            file.content
        ));
    }
    out.push_str("</project_context>\n");
    out
}

/// Best-effort canonicalization for dedup. Mirrors pi's `canonicalizePath`
/// intent (resolve symlinks + normalize) but without a syscall round-trip when
/// the path is already absolute: we normalize separators and collapse `.`/`..`
/// lexically, which is sufficient to dedup the ancestor walk (the same file is
/// never reached via two non-canonical spellings within a single walk).
fn canonical(path: &Path) -> String {
    let normalized = path
        .components()
        .filter(|c| {
            use std::path::Component::*;
            !matches!(c, CurDir | ParentDir)
        })
        .fold(PathBuf::new(), |mut acc, c| {
            acc.push(c.as_os_str());
            acc
        });
    normalized.to_string_lossy().replace('\\', "/")
}

/// Escape a path for safe interpolation into a `path="..."` attribute. The
/// candidate filenames are fixed (`AGENTS.md` etc.), but the directory
/// component is user-controlled, so escape the XML/quote metacharacters.
fn escape_path(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '"' => "&quot;".to_string(),
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tools::env::FileSystem;
    use rpi_tools::in_memory::InMemoryExecutionEnv;

    /// Build a fresh in-memory env with cwd `/proj` and seed the given files
    /// (relative paths resolve against `/proj`). Returns the env as a trait
    /// object so it passes directly to the loader signatures.
    async fn env_with(files: &[(&str, &str)]) -> Arc<dyn ExecutionEnv> {
        let env = Arc::new(InMemoryExecutionEnv::with_cwd(PathBuf::from("/proj")));
        for (rel, content) in files {
            // `absolute_path` normalizes `.` / `/`-relative against cwd.
            let abs = env.absolute_path(rel, None).await.expect("absolute_path");
            env.seed_file(&abs.to_string_lossy(), content.as_bytes().to_vec())
                .await;
        }
        env
    }

    /// Join path segments with `/` and seed a file. The in-memory env stores
    /// `BTreeMap` keys verbatim and looks them up via `resolve_key` (which
    /// normalizes to forward slashes), so tests must seed with forward-slash
    /// absolute strings — `Path::join` on Windows would insert backslashes and
    /// the lookup would miss.
    async fn seed(env: &InMemoryExecutionEnv, abs_slash_path: &str, content: &str) {
        env.seed_file(abs_slash_path, content.as_bytes().to_vec())
            .await;
    }

    #[tokio::test]
    async fn agents_md_beats_claude_md_in_same_dir() {
        let env = env_with(&[
            ("AGENTS.md", "agents-content"),
            ("CLAUDE.md", "claude-content"),
        ])
        .await;
        let dir = Path::new("/proj");
        let got = load_context_file_from_dir(&env, dir).await.expect("found");
        assert_eq!(got.path.file_name().unwrap(), "AGENTS.md");
        assert_eq!(got.content, "agents-content");
    }

    #[tokio::test]
    async fn override_beats_plain_agents() {
        let env = env_with(&[("AGENTS.md", "plain"), ("AGENTS.override.md", "override")]).await;
        let dir = Path::new("/proj");
        let got = load_context_file_from_dir(&env, dir).await.expect("found");
        assert_eq!(got.path.file_name().unwrap(), "AGENTS.override.md");
        assert_eq!(got.content, "override");
    }

    #[tokio::test]
    async fn claude_fallback_when_no_agents() {
        let env = env_with(&[("CLAUDE.md", "claude-only")]).await;
        let dir = Path::new("/proj");
        let got = load_context_file_from_dir(&env, dir).await.expect("found");
        assert_eq!(got.path.file_name().unwrap(), "CLAUDE.md");
    }

    #[tokio::test]
    async fn none_when_no_candidates() {
        let env = env_with(&[("README.md", "unrelated")]).await;
        let dir = Path::new("/proj");
        assert!(load_context_file_from_dir(&env, dir).await.is_none());
    }

    #[tokio::test]
    async fn project_walk_global_then_deep_last() {
        // Global under /home/rpi/agent, project under /proj + /proj/sub.
        let concrete = InMemoryExecutionEnv::with_cwd(PathBuf::from("/proj/sub"));
        let agent_dir = Path::new("/home/rpi/agent");
        let cwd = Path::new("/proj/sub");
        // Seed with forward-slash absolute paths: `InMemoryExecutionEnv` stores
        // `seed_file` keys verbatim, while `file_info`'s `resolve_key` normalizes
        // `\`→`/` (Windows `Path::join` inserts `\`, which would miss the lookup).
        seed(&concrete, "/home/rpi/agent/AGENTS.md", "GLOBAL").await;
        seed(&concrete, "/proj/sub/AGENTS.md", "SUB").await;
        seed(&concrete, "/proj/AGENTS.md", "ROOT").await;
        let env: Arc<dyn ExecutionEnv> = Arc::new(concrete);

        let files = load_project_context_files(&env, cwd, agent_dir).await;
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        // Global first, then root (shallow), then sub (deepest = cwd) last.
        assert_eq!(contents, vec!["GLOBAL", "ROOT", "SUB"]);
    }

    #[tokio::test]
    async fn dedups_when_global_equals_cwd_dir() {
        // agent_dir == cwd dir → the global load + the cwd step hit the same
        // file; the cwd step must skip it (no duplicate entry).
        let concrete = InMemoryExecutionEnv::with_cwd(PathBuf::from("/x/y"));
        seed(&concrete, "/x/y/AGENTS.md", "only").await;
        let env: Arc<dyn ExecutionEnv> = Arc::new(concrete);
        let dir = Path::new("/x/y");
        let files = load_project_context_files(&env, dir, dir).await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "only");
    }

    #[test]
    fn format_empty_when_no_files() {
        assert_eq!(format_project_context(&[]), "");
    }

    #[test]
    fn format_wraps_each_file() {
        let files = vec![ContextFile {
            path: PathBuf::from("/p/AGENTS.md"),
            content: "do x".to_string(),
        }];
        let out = format_project_context(&files);
        assert!(out.starts_with("\n\n<project_context>\n\n"));
        assert!(out.contains("<project_instructions path=\"/p/AGENTS.md\">"));
        assert!(out.contains("do x"));
        assert!(out.ends_with("</project_context>\n"));
    }
}
