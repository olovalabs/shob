//! Platform application directories, modeled on Zed's `paths` crate.
//!
//! All of Kathamo's durable state lives under these directories:
//!
//! - data:    workspace layout, per-pane scrollback, agent conversations, logs
//! - config:  user-editable `settings.json`
//!
//! `KATHAMO_DATA_DIR` overrides the data directory and derives the config,
//! state and log directories from it (used by tests and portable installs).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static UID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn base_dirs() -> Option<directories::BaseDirs> {
    directories::BaseDirs::new()
}

fn custom_data_dir() -> Option<PathBuf> {
    std::env::var_os("KATHAMO_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Workspace layout, scrollback, conversations and logs.
pub fn data_dir() -> PathBuf {
    if let Some(custom) = custom_data_dir() {
        return custom;
    }
    let Some(base) = base_dirs() else {
        return std::env::temp_dir().join("kathamo");
    };
    if cfg!(target_os = "macos") {
        base.home_dir().join("Library/Application Support/Kathamo")
    } else if cfg!(target_os = "windows") {
        base.data_local_dir().join("Kathamo")
    } else {
        base.data_local_dir().join("kathamo")
    }
}

/// User-editable settings (`settings.json`).
pub fn config_dir() -> PathBuf {
    if let Some(custom) = custom_data_dir() {
        return custom.join("config");
    }
    let Some(base) = base_dirs() else {
        return PathBuf::from(".config/kathamo");
    };
    if cfg!(target_os = "windows") {
        base.config_dir().join("Kathamo")
    } else {
        base.config_dir().join("kathamo")
    }
}

pub fn state_dir() -> PathBuf {
    if let Some(custom) = custom_data_dir() {
        return custom.join("state");
    }
    let Some(base) = base_dirs() else {
        return data_dir().join("state");
    };
    if cfg!(target_os = "macos") {
        base.home_dir().join(".local/state/Kathamo")
    } else if cfg!(target_os = "windows") {
        data_dir()
    } else {
        base.state_dir()
            .map(|dir| dir.join("kathamo"))
            .unwrap_or_else(|| data_dir().join("state"))
    }
}

pub fn logs_dir() -> PathBuf {
    if let Some(custom) = custom_data_dir() {
        return custom.join("logs");
    }
    if cfg!(target_os = "macos") {
        return base_dirs()
            .map(|base| base.home_dir().join("Library/Logs/Kathamo"))
            .unwrap_or_else(|| data_dir().join("logs"));
    }
    data_dir().join("logs")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn workspace_path() -> PathBuf {
    data_dir().join("workspace.json")
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

pub fn rpi_sessions_dir() -> PathBuf {
    data_dir().join("rpi").join("sessions")
}

pub fn conversations_dir() -> PathBuf {
    data_dir().join("conversations")
}

pub fn backups_dir() -> PathBuf {
    data_dir().join("backups")
}

/// Create every directory the app writes to. Safe to call on every launch.
pub fn ensure_dirs() -> io::Result<()> {
    for dir in [
        data_dir(),
        config_dir(),
        state_dir(),
        logs_dir(),
        sessions_dir(),
        conversations_dir(),
        backups_dir(),
    ] {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Workspace files written by Kathamo before the v2 layout (and its Markup
/// predecessor). Used only by the one-time migration.
pub fn legacy_workspace_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dirs) = directories::ProjectDirs::from("com", "Kathamo", "Kathamo") {
        paths.push(dirs.data_dir().join("workspace.json"));
    }
    if let Some(dirs) = directories::ProjectDirs::from("com", "Markup", "Markup") {
        paths.push(dirs.data_dir().join("workspace.json"));
    }
    paths
}

/// Stable, collision-resistant identifier for a pane or session. Persisted in
/// the workspace file, so it must not change for the lifetime of the entity.
pub fn uid(prefix: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = UID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}-{now:x}{:x}{:x}",
        std::process::id(),
        counter & 0xffff
    )
}

/// Move `temp` over `dest`, replacing an existing file. On Unix `rename` is
/// atomic; Windows refuses to overwrite, so the destination is removed first.
pub fn atomic_replace(temp: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        if dest.exists() {
            let _ = std::fs::remove_file(dest);
        }
    }
    std::fs::rename(temp, dest)
}

/// Open a directory in the platform file manager (best effort).
pub fn open_in_file_manager(path: &Path) -> io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = std::process::Command::new("explorer");
        command.arg(path);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    command.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `KATHAMO_DATA_DIR` is process-global; serialize the tests that mutate it.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn uid_has_prefix_and_is_unique() {
        let first = uid("p");
        let second = uid("p");
        assert!(first.starts_with("p-"));
        assert!(second.starts_with("p-"));
        assert_ne!(first, second);
        assert!(first.len() > 10);
    }

    #[test]
    fn ensure_dirs_creates_every_directory() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Uses the override so the test never touches the real app data.
        let root = std::env::temp_dir().join(uid("paths-test"));
        // SAFETY: this test is the only writer of this env var in the process.
        unsafe { std::env::set_var("KATHAMO_DATA_DIR", &root) };
        ensure_dirs().expect("create dirs");
        for dir in [
            data_dir(),
            config_dir(),
            state_dir(),
            logs_dir(),
            sessions_dir(),
            conversations_dir(),
            backups_dir(),
        ] {
            assert!(dir.is_dir(), "{} should exist", dir.display());
            assert!(dir.starts_with(&root));
        }
        unsafe { std::env::remove_var("KATHAMO_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_paths_end_with_kathamo() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("KATHAMO_DATA_DIR") };
        let data = data_dir();
        let name = data
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        assert_eq!(name, "kathamo");
        assert!(workspace_path().ends_with("workspace.json"));
        assert!(settings_path().ends_with("settings.json"));
    }
}
