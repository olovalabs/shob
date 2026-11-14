use super::{
    engine::{Identity, Store},
    storage::{apply_writes, auxiliary, publish_aux, ExperimentalNativeStorage, Inner, Write},
};
use crate::{
    error::{SessionError, SessionResult},
    session::{
        memory::{Clock, SystemClock},
        progress::ProgressState,
        types::*,
    },
};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tokio::sync::Mutex as AsyncMutex;
fn lifecycle() -> Arc<AsyncMutex<()>> {
    static GATE: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(AsyncMutex::new(()))).clone()
}
static HANDLES: OnceLock<Mutex<HashMap<PathBuf, Weak<Inner>>>> = OnceLock::new();
fn handles() -> &'static Mutex<HashMap<PathBuf, Weak<Inner>>> {
    HANDLES.get_or_init(Default::default)
}
fn error(e: impl std::fmt::Display) -> SessionError {
    SessionError::storage(e.to_string())
}
fn create_error(e: Box<dyn std::error::Error>) -> SessionError {
    if e.downcast_ref::<super::engine::ReservationError>()
        .is_some_and(|e| e.0.kind() == std::io::ErrorKind::AlreadyExists)
    {
        SessionError::already_exists("experimental artifact already exists")
    } else {
        error(e)
    }
}
fn addressed(root: &Path, id: &str) -> SessionResult<PathBuf> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(SessionError::invalid_payload(
            "experimental session ID is not a safe artifact name",
        ));
    }
    Ok(root.join(format!("{id}.rpinative")))
}
fn open_store(path: &Path) -> super::engine::Result<(Store, SessionMetadata)> {
    let file = std::fs::symlink_metadata(path)?;
    super::engine::ensure(
        file.file_type().is_file() && !file.file_type().is_symlink(),
        "experimental artifact must be a regular non-symlink file",
    )?;
    // A readonly identity probe cannot play a hot journal. First reject foreign
    // headers without opening SQLite, then allow SQLite to recover our initialized
    // format before querying identity. list() shares this logically-readonly path:
    // it may retire a native crash journal, but never migrates or appends facts.
    super::engine::validate_header(path)?;
    let probe = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    probe.busy_timeout(std::time::Duration::from_secs(5))?;
    super::engine::ensure(
        probe.query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))? == 0x52504951
            && probe.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? == 3,
        "unsupported native fact format",
    )?;
    let identity = probe.query_row(
        "SELECT CASE WHEN length(CAST(session AS BLOB))<=128 THEN session ELSE '' END, CASE WHEN length(CAST(generation AS BLOB))<=128 THEN generation ELSE '' END FROM root WHERE singleton=1",
        [],
        |r| {
            Ok(Identity {
                session: r.get(0)?,
                generation: r.get(1)?,
            })
        },
    )?;
    drop(probe);
    let store = Store::open(path, identity)?;
    let metadata: SessionMetadata = auxiliary(&store.connection, &store.identity, "metadata")?;
    super::engine::ensure(
        metadata.id == store.identity.session,
        "catalog identity mismatch",
    )?;
    let mut progress: ProgressState = auxiliary(&store.connection, &store.identity, "progress")?;
    progress.validate().map_err(SessionError::from)?;
    Ok((store, metadata))
}
/// Explicit, default-off experimental repository. Its directory must be dedicated to this format.
#[derive(Clone)]
pub struct ExperimentalNativeRepo {
    root: PathBuf,
    clock: Arc<dyn Clock>,
}
impl ExperimentalNativeRepo {
    pub async fn new(root: PathBuf) -> SessionResult<Self> {
        Self::with_clock(root, Arc::new(SystemClock)).await
    }
    pub async fn with_clock(root: PathBuf, clock: Arc<dyn Clock>) -> SessionResult<Self> {
        let root = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&root)?;
            std::fs::canonicalize(root)
        })
        .await
        .map_err(error)?
        .map_err(error)?;
        Ok(Self { root, clock })
    }
    async fn create_contents(
        &self,
        options: &SessionCreateOptions,
        mutations: Vec<SessionMutation>,
    ) -> SessionResult<ExperimentalNativeStorage> {
        let gate = lifecycle().lock_owned().await;
        let id = options
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let path = addressed(&self.root, &id)?;
        let clock = self.clock.clone();
        let options = options.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            if path.exists() {
                return Err(SessionError::already_exists(
                    "experimental artifact already exists",
                ));
            }
            let metadata = SessionMetadata {
                id: id.clone(),
                created_at: clock.now_ms(),
                parent_session_id: options.parent_session_id,
            };
            let identity = Identity {
                session: id,
                generation: uuid::Uuid::now_v7().to_string(),
            };
            let store = Store::create_initialized(&path, identity, |db, identity| {
                publish_aux(db, identity, "metadata", &metadata)?;
                publish_aux(db, identity, "progress", &ProgressState::default())?;
                apply_writes(
                    db,
                    identity,
                    mutations.into_iter().map(Write::Raw).collect(),
                    Some(0),
                    clock.as_ref(),
                )?;
                Ok(())
            })
            .map_err(create_error)?;
            let storage = ExperimentalNativeStorage::from_store(store, metadata, clock);
            let mut registry = handles().lock().map_err(error)?;
            // Opportunistic cleanup scans capacity (possibly a past peak); no bucket
            // shrinking, idle/Drop cleanup, persistent registry, or loss of live owners.
            registry.retain(|_, weak| weak.strong_count() != 0);
            registry.insert(path, Arc::downgrade(&storage.inner));
            Ok(storage)
        })
        .await
        .map_err(error)?
    }
}
#[async_trait::async_trait]
impl SessionRepo for ExperimentalNativeRepo {
    type Storage = ExperimentalNativeStorage;
    async fn create(&self, options: &SessionCreateOptions) -> SessionResult<Self::Storage> {
        self.create_contents(options, Vec::new()).await
    }
    async fn open(&self, metadata: &SessionMetadata) -> SessionResult<Self::Storage> {
        let gate = lifecycle().lock_owned().await;
        let path = addressed(&self.root, &metadata.id)?;
        let metadata = metadata.clone();
        let clock = self.clock.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            if !path.exists() {
                return Err(SessionError::not_found("experimental artifact not found"));
            }
            let (store, saved) = open_store(&path).map_err(error)?;
            if saved != metadata {
                return Err(SessionError::invalid_entry(
                    "experimental session metadata mismatch",
                ));
            }
            let mut registry = handles().lock().map_err(error)?;
            if let Some(inner) = registry.get(&path).and_then(Weak::upgrade) {
                if inner.store.lock().map_err(error)?.identity != store.identity {
                    return Err(SessionError::storage(
                        "experimental artifact generation changed with an active handle",
                    ));
                }
                return Ok(ExperimentalNativeStorage {
                    metadata: saved,
                    inner,
                });
            }
            registry.retain(|_, weak| weak.strong_count() != 0);
            let storage = ExperimentalNativeStorage::from_store(store, saved, clock);
            registry.insert(path, Arc::downgrade(&storage.inner));
            Ok(storage)
        })
        .await
        .map_err(error)?
    }
    async fn list(&self) -> SessionResult<Vec<SessionMetadata>> {
        let gate = lifecycle().lock_owned().await;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            let mut result = Vec::new();
            for entry in std::fs::read_dir(&root).map_err(error)? {
                let path = entry.map_err(error)?.path();
                if path.extension().and_then(|s| s.to_str()) != Some("rpinative") {
                    continue;
                }
                // A file with our suffix but a bad format fails closed, not silently hidden.
                let (_, metadata) = open_store(&path).map_err(error)?;
                if addressed(&root, &metadata.id)? != path {
                    return Err(SessionError::storage(
                        "experimental artifact filename/identity mismatch",
                    ));
                }
                result.push(metadata);
            }
            result.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(result)
        })
        .await
        .map_err(error)?
    }
    async fn delete(&self, metadata: &SessionMetadata) -> SessionResult<()> {
        let gate = lifecycle().lock_owned().await;
        let path = addressed(&self.root, &metadata.id)?;
        let metadata = metadata.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            if handles()
                .lock()
                .map_err(error)?
                .get(&path)
                .and_then(Weak::upgrade)
                .is_some()
            {
                return Err(SessionError::storage(
                    "experimental session still has active handles",
                ));
            }
            let (store, saved) = open_store(&path).map_err(error)?;
            if saved != metadata {
                return Err(SessionError::invalid_entry(
                    "experimental metadata mismatch",
                ));
            }
            drop(store);
            // Cross-process deletion is not supported: retain the artifact until an
            // OS lifecycle claim protocol exists; never unlink a foreign writer's DB.
            Err(SessionError::storage(
                "experimental deletion requires an unimplemented cross-process lifecycle claim",
            ))
        })
        .await
        .map_err(error)?
    }
    async fn fork(
        &self,
        source: &SessionMetadata,
        options: &SessionCreateOptions,
        fork: &ForkOptions,
    ) -> SessionResult<Self::Storage> {
        let source = self.open(source).await?;
        let mutations = source.fork_mutations(fork.clone()).await?;
        let mut options = options.clone();
        options.parent_session_id = Some(source.metadata.id.clone());
        self.create_contents(&options, mutations).await
    }
}

#[cfg(test)]
#[path = "repo_tests.rs"]
mod tests;
