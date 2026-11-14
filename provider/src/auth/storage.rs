use crate::runtime;
use crate::types::StoredCredential;
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

fn auth_db_path() -> PathBuf {
    runtime::agent_dir().join("agent.db")
}

pub fn authenticated_providers() -> HashSet<String> {
    let Ok(db) = Connection::open_with_flags(auth_db_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return HashSet::new();
    };
    let Ok(mut statement) =
        db.prepare("SELECT DISTINCT provider FROM auth_credentials WHERE disabled_cause IS NULL")
    else {
        return HashSet::new();
    };
    statement
        .query_map([], |row| row.get(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

fn credentials_file_path() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("KATHAMO_DATA_DIR").filter(|v| !v.is_empty()) {
        return Some(
            PathBuf::from(custom)
                .join("config")
                .join("credentials.json"),
        );
    }
    directories::BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("kathamo").join("credentials.json"))
}

/// Load credentials map from Kathamo's credentials store.
pub fn load_local_credentials() -> HashMap<String, StoredCredential> {
    let Some(path) = credentials_file_path() else {
        return HashMap::new();
    };

    if !path.exists() {
        return HashMap::new();
    }

    let Ok(data) = fs::read_to_string(&path) else {
        return HashMap::new();
    };

    serde_json::from_str(&data).unwrap_or_default()
}

/// Save a credential for a provider in Kathamo's credentials store.
pub fn save_local_credential(provider_id: &str, cred: StoredCredential) -> Result<(), String> {
    let path = credentials_file_path()
        .ok_or_else(|| "Could not determine config directory".to_string())?;

    let _lock = lock_local_credentials(&path)?;
    update_local_credentials(&path, |map| {
        map.insert(provider_id.to_string(), cred);
    })
}

fn lock_local_credentials(path: &Path) -> Result<fs::File, String> {
    let acquire = || -> std::io::Result<fs::File> {
        fs::create_dir_all(path.parent().unwrap())?;
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        lock.lock()?;
        Ok(lock)
    };
    acquire().map_err(|error| format!("Could not lock Kathamo credentials: {error}"))
}

fn update_local_credentials(
    path: &Path,
    update: impl FnOnce(&mut HashMap<String, StoredCredential>),
) -> Result<(), String> {
    let write = || -> Result<(), Box<dyn std::error::Error>> {
        let mut map = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error.into()),
        };
        update(&mut map);
        let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        serde_json::to_writer_pretty(&mut temporary, &map)?;
        temporary.persist(path)?;
        Ok(())
    };
    write().map_err(|error| format!("Could not update Kathamo credentials: {error}"))
}

pub fn get_local_credential(provider_id: &str) -> Option<StoredCredential> {
    load_local_credentials().remove(provider_id)
}

pub fn save_api_key(provider_id: &str, key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("API key cannot be empty".into());
    }
    save_local_credential(
        provider_id,
        StoredCredential::ApiKey {
            key: key.into(),
            source: Some("Stored API key".into()),
        },
    )
}

pub fn disconnect_provider(provider_id: &str) -> Result<(), String> {
    let path = credentials_file_path();
    let _lock = path.as_deref().map(lock_local_credentials).transpose()?;
    if auth_db_path().exists() {
        let remove = || -> rusqlite::Result<()> {
            let db =
                Connection::open_with_flags(auth_db_path(), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            db.execute(
                "DELETE FROM auth_credentials WHERE provider = ?1",
                [provider_id],
            )?;
            Ok(())
        };
        remove().map_err(|error| format!("Could not disconnect the Kathamo provider: {error}"))?;
    }
    if let Some(path) = path.filter(|path| path.exists()) {
        update_local_credentials(&path, |map| {
            map.remove(provider_id);
        })?;
    }
    Ok(())
}
