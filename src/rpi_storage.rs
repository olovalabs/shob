//! Session files owned by Kathamo's embedded RPI conversations.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

fn project_dir(cwd: &Path) -> String {
    let cwd = cwd.to_string_lossy();
    let stripped = cwd
        .strip_prefix('/')
        .or_else(|| cwd.strip_prefix('\\'))
        .unwrap_or(&cwd);
    format!("--{}--", stripped.replace(['/', '\\', ':'], "-"))
}

fn session_file(name: &str, id: &str) -> bool {
    name.split_once('_')
        .and_then(|(_, suffix)| suffix.strip_suffix(".jsonl"))
        == Some(id)
}

/// Copy an old project-local session only when it has not already been copied.
pub fn copy_legacy_session(cwd: &Path, id: &str) -> io::Result<()> {
    copy_legacy_session_to(cwd, id, &crate::paths::rpi_sessions_dir())
}

fn copy_legacy_session_to(cwd: &Path, id: &str, root: &Path) -> io::Result<()> {
    if !valid_id(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid RPI session id",
        ));
    }
    let project = project_dir(cwd);
    let source = cwd.join(".rpi/sessions").join(&project);
    let destination = root.join(project);
    let entries = match fs::read_dir(&source) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !session_file(name, id) || !entry.file_type()?.is_file() {
            continue;
        }
        let target = destination.join(name);
        if target.exists() {
            return Ok(());
        }
        fs::create_dir_all(&destination)?;
        // Copy auxiliary progress before the transcript, which acts as the commit marker.
        let progress = format!("{name}.progress.jsonl");
        let progress_source = source.join(&progress);
        if progress_source.is_file() {
            fs::copy(progress_source, destination.join(progress))?;
        }
        let temp = destination.join(format!(".{name}.{}", crate::paths::uid("copy")));
        fs::copy(entry.path(), &temp)?;
        let result = fs::hard_link(&temp, &target);
        let _ = fs::remove_file(temp);
        match result {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        return Ok(());
    }
    Ok(())
}

/// Delete only files whose exact session id was recorded as owned by a pane.
pub fn delete_sessions(ids: &[String]) -> io::Result<()> {
    delete_sessions_in(&crate::paths::rpi_sessions_dir(), ids)
}

fn delete_sessions_in(root: &Path, ids: &[String]) -> io::Result<()> {
    if ids.iter().any(|id| !valid_id(id)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid RPI session id",
        ));
    }
    let dirs = match fs::read_dir(root) {
        Ok(dirs) => dirs,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for dir in dirs {
        let dir = dir?;
        if !dir.file_type()?.is_dir() {
            continue;
        }
        let entries: Vec<PathBuf> = fs::read_dir(dir.path())?
            .map(|e| e.map(|e| e.path()))
            .collect::<io::Result<_>>()?;
        for path in entries {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if ids.iter().any(|id| {
                let base = name
                    .strip_suffix(".progress.jsonl")
                    .or_else(|| name.strip_suffix(".lock"))
                    .unwrap_or(name);
                session_file(base, id)
            }) {
                match fs::remove_file(&path) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_copy_and_owned_deletion_leave_other_sessions_untouched() {
        let base =
            std::env::temp_dir().join(format!("kathamo-rpi-storage-{}", crate::paths::uid("test")));
        let project = base.join("project");
        let root = base.join("data/rpi/sessions");
        let old = project.join(".rpi/sessions").join(project_dir(&project));
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("date_abc.jsonl"), "old").unwrap();
        fs::write(old.join("date_abc.jsonl.progress.jsonl"), "progress").unwrap();
        copy_legacy_session_to(&project, "abc", &root).unwrap();
        let new = root.join(project_dir(&project));
        assert_eq!(
            fs::read_to_string(new.join("date_abc.jsonl")).unwrap(),
            "old"
        );
        fs::write(new.join("date_other.jsonl"), "unrelated").unwrap();
        fs::write(new.join("date_other_abc.jsonl"), "unrelated").unwrap();
        fs::write(new.join("date_abc.jsonl"), "updated").unwrap();
        copy_legacy_session_to(&project, "abc", &root).unwrap();
        assert_eq!(
            fs::read_to_string(new.join("date_abc.jsonl")).unwrap(),
            "updated"
        );
        delete_sessions_in(&root, &["abc".into()]).unwrap();
        assert!(!new.join("date_abc.jsonl").exists());
        assert!(!new.join("date_abc.jsonl.progress.jsonl").exists());
        assert!(new.join("date_other.jsonl").exists());
        assert!(new.join("date_other_abc.jsonl").exists());
        assert_eq!(
            fs::read_to_string(old.join("date_abc.jsonl")).unwrap(),
            "old"
        );
        fs::remove_dir_all(base).unwrap();
    }
}
