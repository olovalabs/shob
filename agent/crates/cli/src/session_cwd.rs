//! Stored-session working-directory recovery.
//!
//! Port of the upstream `packages/coding-agent/src/core/session-cwd.ts`. A
//! session file records the cwd it was created in; when that directory has
//! since been deleted, restoring the session must either fall back to the
//! current cwd or fail with a clear, actionable message instead of silently
//! operating in a non-existent directory.

use std::fmt;
use std::path::{Path, PathBuf};

/// A session whose recorded working directory no longer exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCwdIssue {
    /// The session file that recorded the missing cwd, when known.
    pub session_file: Option<PathBuf>,
    /// The cwd stored in the session file (does not exist).
    pub session_cwd: PathBuf,
    /// The cwd the process is currently running in (the fallback).
    pub fallback_cwd: PathBuf,
}

/// Detect a missing session cwd. Returns `None` when there is no session file,
/// the stored cwd is empty, or the stored cwd is still a directory. A **file**
/// at the recorded path does not pass: every tool resolves relative paths
/// against the cwd, so adopting a non-directory would break them all.
pub fn get_missing_session_cwd_issue(
    session_file: Option<&Path>,
    session_cwd: &Path,
    fallback_cwd: &Path,
) -> Option<SessionCwdIssue> {
    let session_file = session_file?;
    if session_cwd.as_os_str().is_empty() {
        return None;
    }
    if session_cwd.is_dir() {
        return None;
    }
    Some(SessionCwdIssue {
        session_file: Some(session_file.to_path_buf()),
        session_cwd: session_cwd.to_path_buf(),
        fallback_cwd: fallback_cwd.to_path_buf(),
    })
}

/// The working directory a restored session should adopt.
///
/// `Some` when the session recorded a directory that still exists — restoring a
/// conversation must return the agent to where that conversation happened,
/// because every tool resolves relative paths against the run's cwd. `None` when
/// the record is empty or the path is gone (or is a file, not a directory); the
/// caller then falls back to the launch cwd and tells the user via
/// [`missing_session_cwd_note`].
pub fn resolve_session_cwd(session_cwd: &Path) -> Option<PathBuf> {
    if session_cwd.as_os_str().is_empty() {
        return None;
    }
    session_cwd.is_dir().then(|| session_cwd.to_path_buf())
}

/// The one-line transcript note for a session whose recorded directory is gone.
pub fn missing_session_cwd_note(recorded: &Path, fallback: &Path) -> String {
    format!(
        "This session's directory ({}) no longer exists; continuing in {}.",
        recorded.display(),
        fallback.display()
    )
}

/// The one-line transcript note for a session adopted from another directory.
///
/// Restoring a conversation moves the whole run — tools, system prompt, context
/// files, extensions — to the recorded directory, so the user has to hear about
/// it: a `rpi -c` launched in one project must not silently start editing files
/// in another. Mirrors pi's `Session found in different project: <cwd>` notice.
pub fn adopted_session_cwd_note(launch: &Path, adopted: &Path) -> String {
    format!(
        "Continuing this session in {}, where it was created (launched from {}).",
        adopted.display(),
        launch.display()
    )
}

/// The one-line transcript note for a session restored into an untrusted
/// directory. Trust is re-resolved against the adopted directory, so project
/// settings, skills, and context files from it stay off until the user allows.
pub fn untrusted_session_cwd_note(recorded: &Path) -> String {
    format!(
        "This session's directory ({}) is not trusted, so its project settings, \
         skills, and context files are not loaded. Run /trust yes (or restart \
         with --approve) to allow them.",
        recorded.display()
    )
}

/// One-line error message (native `formatMissingSessionCwdError`).
pub fn format_missing_session_cwd_error(issue: &SessionCwdIssue) -> String {
    let session_file = issue
        .session_file
        .as_ref()
        .map(|p| format!("\nSession file: {}", p.display()))
        .unwrap_or_default();
    format!(
        "Stored session working directory does not exist: {}{}\nCurrent working directory: {}",
        issue.session_cwd.display(),
        session_file,
        issue.fallback_cwd.display()
    )
}

/// Short prompt shown by the startup selector (native
/// `formatMissingSessionCwdPrompt`).
pub fn format_missing_session_cwd_prompt(issue: &SessionCwdIssue) -> String {
    format!(
        "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
        issue.session_cwd.display(),
        issue.fallback_cwd.display()
    )
}

/// Error raised when a session's cwd cannot be resolved and no override was
/// supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSessionCwdError {
    pub issue: SessionCwdIssue,
}

impl fmt::Display for MissingSessionCwdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_missing_session_cwd_error(&self.issue))
    }
}

impl std::error::Error for MissingSessionCwdError {}

/// Fail unless the session cwd exists (native `assertSessionCwdExists`).
pub fn assert_session_cwd_exists(
    session_file: Option<&Path>,
    session_cwd: &Path,
    fallback_cwd: &Path,
) -> Result<(), MissingSessionCwdError> {
    match get_missing_session_cwd_issue(session_file, session_cwd, fallback_cwd) {
        Some(issue) => Err(MissingSessionCwdError { issue }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn no_session_file_is_ok() {
        assert!(
            get_missing_session_cwd_issue(None, Path::new("/whatever"), Path::new("/cwd"))
                .is_none()
        );
    }

    #[test]
    fn existing_dir_is_ok() {
        let dir = std::env::temp_dir();
        assert!(get_missing_session_cwd_issue(Some(Path::new("/s.jsonl")), &dir, &dir).is_none());
    }

    #[test]
    fn empty_stored_cwd_is_ok() {
        assert!(get_missing_session_cwd_issue(
            Some(Path::new("/s.jsonl")),
            Path::new(""),
            Path::new("/cwd")
        )
        .is_none());
    }

    #[test]
    fn missing_dir_reports_issue() {
        let missing = PathBuf::from("/definitely/not/here/xyzzy-1234");
        let issue =
            get_missing_session_cwd_issue(Some(Path::new("/s.jsonl")), &missing, Path::new("/cwd"))
                .expect("issue");
        assert_eq!(issue.session_cwd, missing);
        assert!(format_missing_session_cwd_error(&issue).contains("does not exist"));
        assert!(format_missing_session_cwd_prompt(&issue).contains("continue in current cwd"));
        assert!(assert_session_cwd_exists(
            Some(Path::new("/s.jsonl")),
            &missing,
            Path::new("/cwd")
        )
        .is_err());
    }

    #[test]
    fn resolve_session_cwd_accepts_only_existing_directories() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_session_cwd(dir.path()),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(resolve_session_cwd(Path::new("")), None);
        assert_eq!(
            resolve_session_cwd(Path::new("/definitely/not/here/xyzzy-4321")),
            None
        );

        // A FILE at the recorded path must not be adopted: every tool resolves
        // relative paths against the cwd, and a file is not a directory.
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(resolve_session_cwd(&file), None);
        assert!(
            get_missing_session_cwd_issue(Some(Path::new("/s.jsonl")), &file, dir.path()).is_some(),
            "a file at the recorded path counts as a missing cwd"
        );
    }

    #[test]
    fn missing_and_untrusted_notes_name_the_directories() {
        let note = missing_session_cwd_note(Path::new("/gone/project"), Path::new("/here"));
        assert!(note.contains("/gone/project"), "{note}");
        assert!(note.contains("/here"), "{note}");
        assert!(note.contains("no longer exists"), "{note}");

        let note = untrusted_session_cwd_note(Path::new("/gone/project"));
        assert!(note.contains("/gone/project"), "{note}");
        assert!(note.contains("/trust yes"), "{note}");
    }

    /// Adopting another project's directory is a behaviour change the user must
    /// see: the tools, context files, and extensions all switch projects. A
    /// silent adoption would look like `rpi -c` started editing files in the
    /// launch directory while it is actually working somewhere else.
    #[test]
    fn adopting_a_directory_is_announced_with_both_paths() {
        let note =
            adopted_session_cwd_note(Path::new("/launch/project"), Path::new("/other/project"));
        assert!(note.contains("/other/project"), "{note}");
        assert!(note.contains("/launch/project"), "{note}");
        assert!(note.contains("where it was created"), "{note}");
    }
}
