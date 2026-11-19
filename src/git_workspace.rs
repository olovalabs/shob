//! Non-blocking Git workspace inspection and mutations for worktree and branch switching.
//!
//! All `git` subprocess calls set `GIT_OPTIONAL_LOCKS=0` and `GIT_TERMINAL_PROMPT=0`
//! and must be invoked off the GPUI main thread (except fast `.git/HEAD` reads in
//! [`detect_git_branch`]).

use crate::paths;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BRANCH_CACHE_TTL: Duration = Duration::from_secs(1);

type BranchCache = HashMap<PathBuf, (Instant, Option<String>)>;

static BRANCH_CACHE: LazyLock<Mutex<BranchCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static WORKTREE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub is_main: bool,
    pub is_bare: bool,
    pub is_prunable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    pub is_remote: bool,
    pub remote_name: Option<String>,
    pub is_default: bool,
    pub worktree_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitWorkspaceSnapshot {
    pub is_git_repo: bool,
    pub repo_root: Option<PathBuf>,
    pub current_branch: Option<String>,
    pub head_sha: Option<String>,
    pub is_worktree: bool,
    pub main_worktree_path: Option<PathBuf>,
    pub worktrees: Vec<WorktreeInfo>,
    pub branches: Vec<BranchInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchSelectionTarget {
    SwitchWorktree(PathBuf),
    CheckoutLocal(String),
    CheckoutRemote(String),
}

impl GitWorkspaceSnapshot {
    pub fn secondary_worktrees(&self) -> Vec<&WorktreeInfo> {
        self.worktrees
            .iter()
            .filter(|wt| !wt.is_main && !wt.is_bare && !wt.is_prunable && wt.path.is_dir())
            .collect()
    }
}

pub fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

pub fn invalidate_branch_cache(dir: &Path) {
    let mut cache = BRANCH_CACHE.lock();
    cache.remove(dir);
    if let Ok(canon) = std::fs::canonicalize(dir) {
        cache.remove(&canon);
    }
}

pub fn detect_git_branch(dir: &Path) -> Option<String> {
    let now = Instant::now();
    if let Some((at, branch)) = BRANCH_CACHE.lock().get(dir)
        && now.duration_since(*at) < BRANCH_CACHE_TTL
    {
        return branch.clone();
    }
    let branch = detect_git_branch_uncached(dir);
    BRANCH_CACHE
        .lock()
        .insert(dir.to_path_buf(), (now, branch.clone()));
    branch
}

pub fn detect_git_branch_uncached(dir: &Path) -> Option<String> {
    let mut current = dir;
    loop {
        let git_path = current.join(".git");
        if git_path.is_dir() {
            let head_file = git_path.join("HEAD");
            if let Ok(content) = std::fs::read_to_string(head_file) {
                return parse_head_file(&content);
            }
            return None;
        } else if git_path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&git_path) {
                for line in content.lines() {
                    if let Some(rest) = line.strip_prefix("gitdir:") {
                        let rel = rest.trim();
                        let target = if Path::new(rel).is_absolute() {
                            PathBuf::from(rel)
                        } else {
                            current.join(rel)
                        };
                        let head_file = target.join("HEAD");
                        if let Ok(head_content) = std::fs::read_to_string(head_file) {
                            return parse_head_file(&head_content);
                        }
                    }
                }
            }
            return None;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }
    None
}

fn parse_head_file(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
        Some(branch.to_string())
    } else if !trimmed.is_empty() {
        let short = if trimmed.len() > 7 {
            &trimmed[..7]
        } else {
            trimmed
        };
        Some(short.to_string())
    } else {
        None
    }
}

fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_cmd(dir)
        .args(args)
        .output()
        .map_err(|err| format!("Failed to run git: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        Err(if detail.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            detail
        })
    }
}

pub fn inspect(dir: &Path) -> GitWorkspaceSnapshot {
    if !dir.is_dir() {
        return GitWorkspaceSnapshot::default();
    }
    let Ok(top) = run_git(dir, &["rev-parse", "--show-toplevel"]) else {
        return GitWorkspaceSnapshot::default();
    };
    let repo_root = PathBuf::from(top.trim());
    if repo_root.as_os_str().is_empty() {
        return GitWorkspaceSnapshot::default();
    }

    let current_branch = run_git(&repo_root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|s| !s.is_empty());

    let head_sha = run_git(&repo_root, &["rev-parse", "--short", "HEAD"])
        .ok()
        .filter(|s| !s.is_empty());

    let worktrees = run_git(&repo_root, &["worktree", "list", "--porcelain"])
        .map(|out| parse_worktree_porcelain(&out))
        .unwrap_or_default();

    let main_worktree_path = worktrees
        .iter()
        .find(|wt| wt.is_main)
        .map(|wt| wt.path.clone())
        .or_else(|| Some(repo_root.clone()));

    let is_worktree = main_worktree_path
        .as_ref()
        .is_some_and(|main| !paths_equal(main, &repo_root));

    let default_branch = run_git(
        &repo_root,
        &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|s| s.strip_prefix("origin/").map(ToString::to_string));

    let ref_output = run_git(
        &repo_root,
        &[
            "for-each-ref",
            "--format=%(HEAD)%00%(refname)%00%(worktreepath)%00%(symref)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .or_else(|_| {
        run_git(
            &repo_root,
            &[
                "for-each-ref",
                "--format=%(HEAD)%00%(refname)%00%00%(symref)",
                "refs/heads",
                "refs/remotes",
            ],
        )
    })
    .unwrap_or_default();

    let branches = parse_branches(
        &ref_output,
        &worktrees,
        current_branch.as_deref(),
        default_branch.as_deref(),
    );

    GitWorkspaceSnapshot {
        is_git_repo: true,
        repo_root: Some(repo_root),
        current_branch,
        head_sha,
        is_worktree,
        main_worktree_path,
        worktrees,
        branches,
    }
}

pub fn parse_worktree_porcelain(output: &str) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current: Option<WorktreeInfo> = None;

    for raw_line in output.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            if let Some(entry) = current.take() {
                worktrees.push(entry);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                worktrees.push(entry);
            }
            current = Some(WorktreeInfo {
                path: PathBuf::from(path),
                branch: None,
                head_sha: None,
                is_main: false,
                is_bare: false,
                is_prunable: false,
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(sha) = line.strip_prefix("HEAD ") {
            entry.head_sha = Some(sha.chars().take(7).collect());
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            let branch = branch_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_ref)
                .to_string();
            entry.branch = Some(branch);
        } else if line == "bare" {
            entry.is_bare = true;
        } else if line.starts_with("prunable") {
            entry.is_prunable = true;
        }
    }
    if let Some(entry) = current {
        worktrees.push(entry);
    }

    if let Some(first_non_bare) = worktrees.iter_mut().find(|wt| !wt.is_bare) {
        first_non_bare.is_main = true;
    } else if let Some(first) = worktrees.first_mut() {
        first.is_main = true;
    }

    worktrees
}

fn parse_branches(
    output: &str,
    worktrees: &[WorktreeInfo],
    current_branch: Option<&str>,
    default_branch: Option<&str>,
) -> Vec<BranchInfo> {
    let mut local_names = HashSet::new();
    let mut raw_locals = Vec::new();
    let mut raw_remotes = Vec::new();

    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, '\0');
        let head_marker = parts.next().unwrap_or("");
        let refname = parts.next().unwrap_or("");
        let worktree_str = parts.next().unwrap_or("");
        let symref = parts.next().unwrap_or("");

        if !symref.is_empty() || refname.ends_with("/HEAD") {
            continue;
        }

        if let Some(name) = refname.strip_prefix("refs/heads/") {
            local_names.insert(name.to_string());
            let worktree_path = if !worktree_str.is_empty() {
                Some(PathBuf::from(worktree_str))
            } else {
                worktrees
                    .iter()
                    .find(|wt| wt.branch.as_deref() == Some(name) && !wt.is_prunable)
                    .map(|wt| wt.path.clone())
            };
            let is_current =
                head_marker.trim() == "*" || current_branch == Some(name);
            let is_default = default_branch == Some(name);
            raw_locals.push(BranchInfo {
                name: name.to_string(),
                is_current,
                is_remote: false,
                remote_name: None,
                is_default,
                worktree_path,
            });
        } else if let Some(remote_full) = refname.strip_prefix("refs/remotes/") {
            let (remote_name, short_name) = match remote_full.split_once('/') {
                Some((remote, short)) => (Some(remote.to_string()), short),
                None => (None, remote_full),
            };
            raw_remotes.push((
                remote_full.to_string(),
                short_name.to_string(),
                remote_name,
            ));
        }
    }

    let mut branches = raw_locals;
    for (full_name, short_name, remote_name) in raw_remotes {
        if local_names.contains(&short_name) {
            continue;
        }
        branches.push(BranchInfo {
            name: full_name,
            is_current: false,
            is_remote: true,
            remote_name,
            is_default: false,
            worktree_path: None,
        });
    }

    branches.sort_by(|a, b| {
        b.is_current
            .cmp(&a.is_current)
            .then_with(|| a.is_remote.cmp(&b.is_remote))
            .then_with(|| b.is_default.cmp(&a.is_default))
            .then_with(|| b.worktree_path.is_some().cmp(&a.worktree_path.is_some()))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    branches
}

pub fn resolve_branch_selection(
    branches: &[BranchInfo],
    branch_name: &str,
    current_dir: &Path,
) -> Option<BranchSelectionTarget> {
    let branch = branches.iter().find(|b| b.name == branch_name)?;
    if let Some(worktree_path) = &branch.worktree_path
        && !paths_equal(worktree_path, current_dir)
    {
        return Some(BranchSelectionTarget::SwitchWorktree(worktree_path.clone()));
    }
    if branch.is_remote {
        Some(BranchSelectionTarget::CheckoutRemote(branch.name.clone()))
    } else {
        Some(BranchSelectionTarget::CheckoutLocal(branch.name.clone()))
    }
}

pub fn sanitize_new_ref_name(raw: &str) -> String {
    let collapsed = raw
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let trimmed = collapsed.trim_start_matches('-').trim_matches('/');
    let trimmed = trimmed.trim_end_matches('.');
    if trimmed.is_empty()
        || trimmed.contains("..")
        || trimmed.contains("@{")
        || trimmed.ends_with(".lock")
        || trimmed.chars().any(|c| {
            c.is_ascii_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
    {
        return String::new();
    }
    trimmed.to_string()
}

fn sanitize_dir_segment(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(['.', '-']);
    if trimmed.is_empty() {
        "repo".to_string()
    } else {
        trimmed.to_string()
    }
}

fn next_worktree_hex() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let seq = WORKTREE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = nanos ^ ((std::process::id() as u64) << 16) ^ seq.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    format!("{:08x}", (mixed & 0xffff_ffff) as u32)
}

pub fn create_worktree(
    dir: &Path,
    worktrees_root: &Path,
    base_branch: Option<&str>,
) -> Result<WorktreeInfo, String> {
    let snapshot = inspect(dir);
    let repo_root = snapshot
        .repo_root
        .as_deref()
        .ok_or_else(|| "This folder is not a Git repository.".to_string())?;

    if run_git(repo_root, &["rev-parse", "--verify", "HEAD"]).is_err() {
        return Err(
            "This repository has no commits yet. Create an initial commit before adding a worktree."
                .to_string(),
        );
    }

    let main_root = snapshot
        .main_worktree_path
        .as_deref()
        .unwrap_or(repo_root);
    let repo_name = main_root
        .file_name()
        .map(|n| sanitize_dir_segment(&n.to_string_lossy()))
        .unwrap_or_else(|| "repo".to_string());

    let repo_worktrees_dir = worktrees_root.join(repo_name);
    std::fs::create_dir_all(&repo_worktrees_dir)
        .map_err(|err| format!("Failed to prepare worktree directory: {err}"))?;

    // Avoid ref prefix conflicts if `refs/heads/shob` exists as a branch file.
    let use_slash = run_git(repo_root, &["show-ref", "--verify", "--quiet", "refs/heads/shob"]).is_err();

    let mut chosen_branch = String::new();
    let mut chosen_path = PathBuf::new();
    for _ in 0..8 {
        let token = next_worktree_hex();
        let candidate_branch = if use_slash {
            format!("shob/{token}")
        } else {
            format!("shob-{token}")
        };
        let dir_name = candidate_branch.replace('/', "-");
        let candidate_path = repo_worktrees_dir.join(dir_name);
        let branch_exists = run_git(
            repo_root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate_branch}"),
            ],
        )
        .is_ok();
        if !candidate_path.exists() && !branch_exists {
            chosen_branch = candidate_branch;
            chosen_path = candidate_path;
            break;
        }
    }

    if chosen_branch.is_empty() {
        let fallback_uid = paths::uid("wt");
        chosen_branch = if use_slash {
            format!("shob/{fallback_uid}")
        } else {
            format!("shob-{fallback_uid}")
        };
        chosen_path = repo_worktrees_dir.join(chosen_branch.replace('/', "-"));
    }

    let start_point = base_branch
        .filter(|b| !b.is_empty() && run_git(repo_root, &["rev-parse", "--verify", b]).is_ok())
        .unwrap_or("HEAD");

    let target_str = chosen_path.to_string_lossy().to_string();
    run_git(
        repo_root,
        &[
            "worktree",
            "add",
            "-b",
            &chosen_branch,
            "--",
            &target_str,
            start_point,
        ],
    )?;

    let head_sha = run_git(&chosen_path, &["rev-parse", "--short", "HEAD"]).ok();
    invalidate_branch_cache(dir);
    invalidate_branch_cache(repo_root);
    invalidate_branch_cache(&chosen_path);

    Ok(WorktreeInfo {
        path: chosen_path,
        branch: Some(chosen_branch),
        head_sha,
        is_main: false,
        is_bare: false,
        is_prunable: false,
    })
}

pub fn checkout_branch(dir: &Path, branch: &str, is_remote: bool) -> Result<String, String> {
    if branch.is_empty() || branch.starts_with('-') {
        return Err("Invalid branch name.".to_string());
    }
    let checked_out = if is_remote {
        let local = branch
            .split_once('/')
            .map_or(branch, |(_, rest)| rest);
        let local_ref = format!("refs/heads/{local}");
        if run_git(dir, &["show-ref", "--verify", "--quiet", &local_ref]).is_ok() {
            run_git(dir, &["checkout", local])?;
        } else {
            run_git(dir, &["checkout", "--track", branch])?;
        }
        local.to_string()
    } else {
        run_git(dir, &["checkout", branch])?;
        branch.to_string()
    };

    invalidate_branch_cache(dir);
    Ok(checked_out)
}

pub fn create_and_checkout_branch(dir: &Path, raw_name: &str) -> Result<String, String> {
    let name = sanitize_new_ref_name(raw_name);
    if name.is_empty() {
        return Err("Invalid branch name.".to_string());
    }
    let full_ref = format!("refs/heads/{name}");
    run_git(dir, &["check-ref-format", &full_ref])
        .map_err(|_| format!("Invalid branch name: {name}"))?;
    run_git(dir, &["checkout", "-b", &name])?;
    invalidate_branch_cache(dir);
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Shob Test")
            .env("GIT_AUTHOR_EMAIL", "test@shob.local")
            .env("GIT_COMMITTER_NAME", "Shob Test")
            .env("GIT_COMMITTER_EMAIL", "test@shob.local")
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {:?} failed in {}", args, dir.display());
    }

    #[test]
    fn parses_worktree_porcelain_output() {
        let sample = "\
worktree /repo/main
HEAD 1234567890abcdef
branch refs/heads/main

worktree /data/worktrees/repo/shob-a1b2c3d4
HEAD fedcba0987654321
branch refs/heads/shob/a1b2c3d4

worktree /data/worktrees/repo/detached
HEAD 0011223344556677
detached
";
        let worktrees = parse_worktree_porcelain(sample);
        assert_eq!(worktrees.len(), 3);
        assert!(worktrees[0].is_main);
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert_eq!(worktrees[0].head_sha.as_deref(), Some("1234567"));
        assert!(!worktrees[1].is_main);
        assert_eq!(worktrees[1].branch.as_deref(), Some("shob/a1b2c3d4"));
        assert!(!worktrees[2].is_main);
        assert_eq!(worktrees[2].branch, None);
    }

    #[test]
    fn sanitizes_new_branch_names() {
        assert_eq!(sanitize_new_ref_name("  feat  my branch "), "feat-my-branch");
        assert_eq!(sanitize_new_ref_name("--flag"), "flag");
        assert_eq!(sanitize_new_ref_name("feat..bad"), "");
        assert_eq!(sanitize_new_ref_name("bad.lock"), "");
        assert_eq!(sanitize_new_ref_name("bad:name"), "");
        assert_eq!(sanitize_new_ref_name("feat/v2"), "feat/v2");
    }

    #[test]
    fn resolves_branch_target_to_other_worktree_when_checked_out_elsewhere() {
        let branches = vec![
            BranchInfo {
                name: "main".into(),
                is_current: false,
                is_remote: false,
                remote_name: None,
                is_default: true,
                worktree_path: Some(PathBuf::from("/repo/main")),
            },
            BranchInfo {
                name: "feat/x".into(),
                is_current: true,
                is_remote: false,
                remote_name: None,
                is_default: false,
                worktree_path: Some(PathBuf::from("/repo/wt-x")),
            },
            BranchInfo {
                name: "origin/other".into(),
                is_current: false,
                is_remote: true,
                remote_name: Some("origin".into()),
                is_default: false,
                worktree_path: None,
            },
        ];

        assert_eq!(
            resolve_branch_selection(&branches, "main", Path::new("/repo/wt-x")),
            Some(BranchSelectionTarget::SwitchWorktree(PathBuf::from(
                "/repo/main"
            )))
        );
        assert_eq!(
            resolve_branch_selection(&branches, "feat/x", Path::new("/repo/wt-x")),
            Some(BranchSelectionTarget::CheckoutLocal("feat/x".into()))
        );
        assert_eq!(
            resolve_branch_selection(&branches, "origin/other", Path::new("/repo/wt-x")),
            Some(BranchSelectionTarget::CheckoutRemote(
                "origin/other".into()
            ))
        );
    }

    #[test]
    fn inspect_create_worktree_and_branch_checkout_on_real_repo() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("demo");
        let wt_root = temp.path().join("worktrees");
        std::fs::create_dir_all(&repo).unwrap();

        git(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "initial"]);

        let snap = inspect(&repo);
        assert!(snap.is_git_repo);
        assert!(!snap.is_worktree);
        assert_eq!(snap.current_branch.as_deref(), Some("main"));
        assert_eq!(snap.worktrees.len(), 1);
        assert!(snap.secondary_worktrees().is_empty());

        let wt = create_worktree(&repo, &wt_root, None).expect("create worktree");
        assert!(wt.path.is_dir());
        assert!(wt.branch.as_deref().is_some_and(|b| b.starts_with("shob/")));
        assert_eq!(
            detect_git_branch_uncached(&wt.path),
            wt.branch
        );

        let wt_snap = inspect(&wt.path);
        assert!(wt_snap.is_git_repo);
        assert!(wt_snap.is_worktree);
        assert_eq!(wt_snap.current_branch, wt.branch);
        assert_eq!(wt_snap.worktrees.len(), 2);
        assert_eq!(wt_snap.secondary_worktrees().len(), 1);

        // Branch checked out in main worktree resolves to switching worktrees.
        assert_eq!(
            resolve_branch_selection(&wt_snap.branches, "main", &wt.path),
            Some(BranchSelectionTarget::SwitchWorktree(
                wt_snap.main_worktree_path.clone().unwrap()
            ))
        );

        let created = create_and_checkout_branch(&wt.path, "feat new api").expect("new branch");
        assert_eq!(created, "feat-new-api");
        assert_eq!(
            detect_git_branch_uncached(&wt.path).as_deref(),
            Some("feat-new-api")
        );

        // `main` cannot be checked out directly while checked out in main worktree.
        assert!(checkout_branch(&wt.path, "main", false).is_err());
        let switched_back = checkout_branch(&wt.path, wt.branch.as_deref().unwrap(), false)
            .expect("checkout existing branch");
        assert_eq!(Some(switched_back), wt.branch);
    }
}
