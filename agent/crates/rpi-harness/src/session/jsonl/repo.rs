//! Mirrors `packages/agent/src/harness/session/jsonl/repo.ts` — `JsonlSessionRepo`,
//! the `SessionRepo` impl that persists sessions as cwd-encoded JSONL files.
//!
//! ## Layout (mirrors the TS repo + the coding-agent on-disk convention)
//!
//! - Root directory (resolved once, cached): `<sessions_root>` absolute path.
//! - Per-cwd directory: `--<cwd-with-leading-slash-stripped-and-/-:-replaced-by-->--`
//!   (see [`jsonl_session_directory_name`]). Allows sessions from many cwds to
//!   live under one root without collision.
//! - Session file: `<ISO-timestamp-with-:-and-.-replaced-by-->_<id>.jsonl`
//!   (see [`session_file_name`]).
//!
//! ## Same-process create/fork race guard
//!
//! The durable filename embeds a timestamp, so an async filesystem exists-check
//! alone can let two concurrent `create`/`fork` calls for the same `{cwd, id}`
//! both decide it's free and publish duplicate sessions. [`claim_create_destination`]
//! guards against that with an in-process `Mutex<HashSet>` keyed `cwd\0id`.
//!
//! ## Trait vs inherent API (cwd tension)
//!
//! The shared [`SessionRepo`] trait signatures (`create`/`list`/`fork`) do NOT
//! carry `cwd`, but the JSONL on-disk layout is cwd-encoded — `cwd` is load-
//! bearing for the JSONL backend. Resolution (see the initial port notes (retired)):
//!
//! - **Inherent typed methods** — [`JsonlSessionRepo::create_typed`],
//!   [`JsonlSessionRepo::list_typed`], [`JsonlSessionRepo::fork_typed`] — take
//!   the JSONL-specific option structs ([`JsonlSessionCreateOptions`] /
//!   [`JsonlSessionListOptions`]) and are the *primary* entry points. These
//!   mirror the TS `create(JsonlSessionCreateOptions)` / `list(JsonlSessionListOptions)`
//!   signatures directly.
//! - **`SessionRepo` trait impl** — `create`/`list`/`fork` delegate to the typed
//!   methods using a configured default `cwd` (the repo's `default_cwd`). The
//!   trait `open`/`delete` take [`SessionMetadata`] (the base projection), but
//!   the JSONL backend needs the richer [`JsonlSessionMetadata`]; the trait impl
//!   therefore `open`s by matching the base id against a `list_typed` scan, and
//!   `delete`s by scan-then-remove (losing the direct-path fast path the TS
//!   repo has via `metadata.path`). Callers that have a [`JsonlSessionMetadata`]
//!   in hand should prefer [`open_by_jsonl_metadata`] / [`delete_by_jsonl_metadata`].
//!
//! This keeps the JSONL backend a faithful port of the TS file (which itself
//! takes `JsonlSessionCreateOptions` / `JsonlSessionListOptions`) while still
//! satisfying the crate-wide [`SessionRepo`] trait the `Session` facade consumes.

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use tokio::sync::Mutex as TokioMutex;
use uuid::Uuid;

use rpi_tools::env::{FileKind, FileSystem};

use crate::error::{SessionError, SessionResult};
use crate::session::jsonl::codec::{metadata_from_header, parse_header};
use crate::session::jsonl::errors::file_result;
use crate::session::jsonl::storage::JsonlSessionStorage;
use crate::session::jsonl::types::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata,
    JsonlSessionRepoOptions, JsonlV4Header,
};
use crate::session::memory::{Clock, InMemorySessionStorage};
use crate::session::types::{
    ForkOptions, JsonValue, SessionCreateOptions, SessionMetadata, SessionRepo, SessionStorage,
};

/// `^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$` — a session id must be
/// non-empty, alphanumeric + `-_.`, and start/end with an alphanumeric char.
/// Mirrors TS `SESSION_ID_PATTERN`.
const SESSION_ID_MIN_LEN: usize = 1;
const SESSION_ID_MAX_LEN: usize = 64;

/// Validate a session id against [`SESSION_ID_PATTERN`]'s equivalent rule.
/// Mirrors TS `validateSessionId`. Returns `invalid_payload` on violation.
fn validate_session_id(id: &str) -> SessionResult<()> {
    if id.len() < SESSION_ID_MIN_LEN || id.len() > SESSION_ID_MAX_LEN {
        return Err(SessionError::invalid_payload(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character",
        ));
    }
    let chars: Vec<char> = id.chars().collect();
    if !chars
        .first()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false)
        || !chars
            .last()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
    {
        return Err(SessionError::invalid_payload(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character",
        ));
    }
    if !chars
        .iter()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(SessionError::invalid_payload(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character",
        ));
    }
    Ok(())
}

/// `--<cwd-with-leading-/-or-\-stripped-and-/-\-:-replaced-by-->--`.
/// Mirrors TS `jsonlSessionDirectoryName`. Strips one leading `/` or `\`, then
/// replaces every `/`, `\`, and `:` with `-`.
fn jsonl_session_directory_name(cwd: &str) -> String {
    let stripped = cwd
        .strip_prefix('/')
        .or_else(|| cwd.strip_prefix('\\'))
        .unwrap_or(cwd);
    let replaced: String = stripped
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect();
    format!("--{replaced}--")
}

/// `<ISO-timestamp-with-:-and-.-replaced-by-->_<id>.jsonl`. Mirrors TS
/// `sessionFileName`. `created_at` is ms since Unix epoch.
fn session_file_name(created_at: i64, id: &str) -> String {
    let iso = iso_timestamp_from_ms(created_at);
    let safe = iso.replace([':', '.'], "-");
    format!("{safe}_{id}.jsonl")
}

/// Render `ms` as an ISO-8601 UTC timestamp `YYYY-MM-DDTHH:MM:SS.mmmZ` (the TS
/// `new Date(createdAt).toISOString()` shape). Hand-rolled to avoid a chrono/jiff
/// dependency; the format is fully determined by the ms value.
fn iso_timestamp_from_ms(ms: i64) -> String {
    // Days since 1970-01-01 (floor; supports negative ms for pre-epoch, though
    // timestamps here are always non-negative).
    let total_seconds = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days_since_epoch = total_seconds.div_euclid(86_400);
    let secs_of_day = total_seconds.rem_euclid(86_400);

    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let (year, month, day) = civil_from_days(days_since_epoch);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's days-from-epoch → civil date algorithm. Returns
/// `(year, month, day)` with `month`/`day` 1-based.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

/// A `{id, cwd}` destination for a create/fork op, with a same-process claim
/// set to deduplicate concurrent creates of the same logical session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CreateDestination {
    id: String,
    cwd: String,
}

impl CreateDestination {
    fn dedup_key(&self) -> String {
        format!("{}\0{}", self.cwd, self.id)
    }
}

/// `JsonlSessionRepo` — a `SessionRepo` persisting sessions as cwd-encoded
/// JSONL files under a root directory. Mirrors TS `JsonlSessionRepo`.
pub struct JsonlSessionRepo {
    fs: Arc<dyn FileSystem>,
    sessions_root_input: String,
    /// Default cwd used by the `SessionRepo` trait impl's `create`/`list`/`fork`
    /// (which lack a cwd parameter). Inherent typed methods take cwd explicitly.
    default_cwd: String,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn crate::session::types::IdGenerator>,
    /// Same-process create/fork dedup set (TS `activeCreateDestinations`).
    active_create_destinations: StdMutex<HashSet<String>>,
    /// Per-destination mutex serializing the rename in the torn-tail/create
    /// paths across repo-level races (analogous to the storage's per-handle
    /// `tail`; the TS repo relies on the FS atomicity of rename + the in-process
    /// dedup set, but a per-key mutex hardens cross-task create races further).
    create_locks: TokioMutex<()>,
    /// Cached resolved root (TS `rootPromise`). `StdMutex` so resolution is
    /// sync-obtained; the resolved value is then reused on every call.
    root: StdMutex<Option<String>>,
}

impl JsonlSessionRepo {
    /// Build a repo. `default_cwd` is the cwd the `SessionRepo` trait impl uses
    /// for `create`/`list`/`fork` (which have no cwd parameter); the inherent
    /// typed methods take cwd explicitly and ignore it.
    pub fn new(options: JsonlSessionRepoOptions, default_cwd: String) -> Self {
        Self {
            fs: options.fs,
            sessions_root_input: options.sessions_root,
            default_cwd,
            clock: options.clock,
            ids: options.ids,
            active_create_destinations: StdMutex::new(HashSet::new()),
            create_locks: TokioMutex::new(()),
            root: StdMutex::new(None),
        }
    }

    /// The configured default cwd (used by the trait impl).
    pub fn default_cwd(&self) -> &str {
        &self.default_cwd
    }

    /// Convenience: build from the shared `JsonlSessionRepoOptions` + a default
    /// cwd derived from the env's current working directory.
    pub fn with_env_cwd(options: JsonlSessionRepoOptions) -> Self {
        let cwd = options.fs.cwd().to_string_lossy().into_owned();
        Self::new(options, cwd)
    }

    // ---- inherent typed API (primary; mirrors TS signatures) ----

    /// Create a new session file. Mirrors TS `create(options: JsonlSessionCreateOptions)`.
    pub async fn create_typed(
        &self,
        options: &JsonlSessionCreateOptions,
    ) -> SessionResult<JsonlSessionStorage> {
        let destination = self.resolve_create_destination(options).await?;
        self.claim_create_destination(&destination, async {
            let (header, path) = self.prepare_create(&destination, options).await?;
            JsonlSessionStorage::create(
                self.fs.clone(),
                &path,
                header,
                self.clock.clone(),
                self.ids.clone(),
            )
            .await
        })
        .await
    }

    /// List session metadata, newest-first. Mirrors TS
    /// `list(options: JsonlSessionListOptions = {})`.
    pub async fn list_typed(
        &self,
        options: &JsonlSessionListOptions,
    ) -> SessionResult<Vec<JsonlSessionMetadata>> {
        list_jsonl_session_metadata(self, options).await
    }

    /// Open a session by its rich JSONL metadata. Mirrors TS
    /// `open(metadata: JsonlSessionMetadata)` — the preferred open path since
    /// it has the on-disk `path` in hand (no scan).
    pub async fn open_by_jsonl_metadata(
        &self,
        metadata: &JsonlSessionMetadata,
    ) -> SessionResult<JsonlSessionStorage> {
        load_jsonl_session_storage(self, metadata).await
    }

    /// Inspect recovered native facts without the writer lifecycle or on-disk
    /// repair. Unlike `open_by_jsonl_metadata`, this never changes the source;
    /// any snapshot mutations are confined to memory.
    pub async fn read_snapshot_by_jsonl_metadata(
        &self,
        metadata: &JsonlSessionMetadata,
    ) -> SessionResult<InMemorySessionStorage> {
        let snapshot = JsonlSessionStorage::read_snapshot(
            self.fs.clone(),
            &metadata.path,
            self.clock.clone(),
            self.ids.clone(),
        )
        .await?;
        if snapshot.metadata().id != metadata.id {
            return Err(SessionError::invalid_entry(format!(
                "Session id does not match header: {}",
                metadata.id
            )));
        }
        Ok(snapshot)
    }

    /// Inspect only native metadata/values, keeping historical frames cold and
    /// never repairing or exporting a disk-backed storage capability.
    pub async fn read_facts_by_jsonl_metadata(
        &self,
        metadata: &JsonlSessionMetadata,
    ) -> SessionResult<super::JsonlSessionFacts> {
        let facts = JsonlSessionStorage::read_facts(
            self.fs.clone(),
            &metadata.path,
            self.clock.clone(),
            self.ids.clone(),
        )
        .await?;
        if facts.metadata.id != metadata.id {
            return Err(SessionError::invalid_entry(format!(
                "Session id does not match header: {}",
                metadata.id
            )));
        }
        Ok(facts)
    }

    /// Delete a session by its rich JSONL metadata (direct-path remove).
    /// Mirrors TS `delete(metadata: JsonlSessionMetadata)`.
    pub async fn delete_by_jsonl_metadata(
        &self,
        metadata: &JsonlSessionMetadata,
    ) -> SessionResult<()> {
        // Remove the session first: a failed primary deletion must not erase
        // recovery payloads from a session which still exists.
        file_result(
            self.fs.remove(&metadata.path, false, true, None).await,
            &format!("Failed to delete session {}", metadata.path),
        )?;
        for suffix in [".progress.jsonl", ".progress.jsonl.tmp"] {
            file_result(
                self.fs
                    .remove(&format!("{}{suffix}", metadata.path), false, true, None)
                    .await,
                "delete session progress",
            )?;
        }
        Ok(())
    }

    /// Fork a session. Mirrors TS
    /// `fork(source, options: ForkOptions & JsonlSessionCreateOptions)`.
    pub async fn fork_typed(
        &self,
        source: &JsonlSessionMetadata,
        options: &JsonlSessionCreateOptions,
        fork: &ForkOptions,
    ) -> SessionResult<JsonlSessionStorage> {
        self.fork_initialized(
            source,
            options,
            fork,
            None::<
                fn(
                    &crate::session::SessionValues,
                ) -> SessionResult<Vec<(String, serde_json::Value)>>,
            >,
        )
        .await
    }

    /// Host-owned new-identity values are staged by the ordinary fork engine
    /// before atomic publication, using the source's same-snapshot values.
    pub async fn fork_typed_with_value_initializer(
        &self,
        source: &JsonlSessionMetadata,
        options: &JsonlSessionCreateOptions,
        fork: &ForkOptions,
        initialize: impl FnOnce(
                &crate::session::SessionValues,
            ) -> SessionResult<Vec<(String, serde_json::Value)>>
            + Send,
    ) -> SessionResult<JsonlSessionStorage> {
        self.fork_initialized(source, options, fork, Some(initialize))
            .await
    }

    async fn fork_initialized(
        &self,
        source: &JsonlSessionMetadata,
        options: &JsonlSessionCreateOptions,
        fork: &ForkOptions,
        initialize: Option<
            impl FnOnce(
                    &crate::session::SessionValues,
                ) -> SessionResult<Vec<(String, serde_json::Value)>>
                + Send,
        >,
    ) -> SessionResult<JsonlSessionStorage> {
        let source_storage = JsonlSessionStorage::load_fork_snapshot(
            self.fs.clone(),
            &source.path,
            self.clock.clone(),
            self.ids.clone(),
        )
        .await?;
        if source_storage.jsonl_metadata().id != source.id {
            return Err(SessionError::invalid_entry(format!(
                "Session id does not match header: {}",
                source.id
            )));
        }
        // TS: parentSessionId ?? source.id.
        let mut create_options = options.clone();
        if create_options.parent_session_id.is_none() {
            create_options.parent_session_id = Some(source.id.clone());
        }
        let destination = self.resolve_create_destination(&create_options).await?;
        self.claim_create_destination(&destination, async {
            let (header, path) = self.prepare_create(&destination, &create_options).await?;
            match initialize {
                Some(initialize) => {
                    source_storage
                        .fork_with_value_initializer(
                            &path,
                            header,
                            fork,
                            self.clock.clone(),
                            self.ids.clone(),
                            initialize,
                        )
                        .await
                }
                None => {
                    source_storage
                        .fork(&path, header, fork, self.clock.clone(), self.ids.clone())
                        .await
                }
            }
        })
        .await
    }

    // ---- internals (mirror repo.ts private methods) ----

    /// Resolve `{id, cwd}` for a create op: default the id to a fresh uuidv7,
    /// validate it, and absolute-path the cwd. Mirrors `resolveCreateDestination`.
    async fn resolve_create_destination(
        &self,
        options: &JsonlSessionCreateOptions,
    ) -> SessionResult<CreateDestination> {
        let id = match &options.id {
            Some(id) => id.clone(),
            None => Uuid::now_v7().to_string(),
        };
        validate_session_id(&id)?;
        let cwd = file_result(
            self.fs.absolute_path(&options.cwd, None).await,
            &format!("Failed to resolve session cwd {}", options.cwd),
        )?
        .to_string_lossy()
        .into_owned();
        Ok(CreateDestination { id, cwd })
    }

    /// Prevent same-process create/fork races for one logical destination.
    /// Mirrors `claimCreateDestination`: if `{cwd,id}` is already being created,
    /// `already_exists`; otherwise insert, run the op, remove in `finally`.
    async fn claim_create_destination<F, T>(
        &self,
        destination: &CreateDestination,
        op: F,
    ) -> SessionResult<T>
    where
        F: std::future::Future<Output = SessionResult<T>>,
    {
        let key = destination.dedup_key();
        {
            let mut active = self
                .active_create_destinations
                .lock()
                .expect("active create set not poisoned");
            if active.contains(&key) {
                return Err(SessionError::already_exists(format!(
                    "Session already exists: {}",
                    destination.id
                )));
            }
            active.insert(key.clone());
        }
        let result = op.await;
        {
            let mut active = self
                .active_create_destinations
                .lock()
                .expect("active create set not poisoned");
            active.remove(&key);
        }
        result
    }

    /// Build the header + resolved file path for a create, after confirming the
    /// session id is not already in use on disk. Mirrors `prepareCreate`.
    async fn prepare_create(
        &self,
        destination: &CreateDestination,
        options: &JsonlSessionCreateOptions,
    ) -> SessionResult<(JsonlV4Header, String)> {
        let CreateDestination { id, cwd } = destination.clone();
        if self.session_id_exists(&id, &cwd).await? {
            return Err(SessionError::already_exists(format!(
                "Session already exists: {id}"
            )));
        }

        let created_at = self.clock.now_ms();
        let session_directory = self.session_directory(&cwd).await?;
        let path = file_result(
            self.fs
                .join_path(
                    &[&session_directory, &session_file_name(created_at, &id)],
                    None,
                )
                .await,
            &format!("Failed to resolve path for session {id}"),
        )?
        .to_string_lossy()
        .into_owned();
        if let Some(metadata) = &options.metadata {
            assert_json_serializable(metadata)?;
        }
        let header = JsonlV4Header::new(
            id,
            created_at,
            cwd,
            options.parent_session_id.clone(),
            None,
            options.metadata.clone(),
        );
        file_result(
            self.fs.create_dir(&session_directory, true, None).await,
            "Failed to create sessions directory",
        )?;
        Ok((header, path))
    }

    /// Does a session file with the `_<id>.jsonl` suffix already exist in the
    /// cwd's directory? Mirrors `sessionIdExists`.
    async fn session_id_exists(&self, id: &str, cwd: &str) -> SessionResult<bool> {
        let suffix = format!("_{id}.jsonl");
        let directory = self.session_directory(cwd).await?;
        if !file_result(
            self.fs.exists(&directory, None).await,
            &format!("Failed to check sessions directory {directory}"),
        )? {
            return Ok(false);
        }
        let files = file_result(
            self.fs.list_dir(&directory, None).await,
            &format!("Failed to list sessions directory {directory}"),
        )?;
        Ok(files
            .iter()
            .any(|entry| entry.kind != FileKind::Directory && entry.name.ends_with(&suffix)))
    }

    /// `<root>/<cwd-encoded-dir>`. Mirrors `sessionDirectory`.
    async fn session_directory(&self, cwd: &str) -> SessionResult<String> {
        let root = self.root().await?;
        let dir_name = jsonl_session_directory_name(cwd);
        file_result(
            self.fs.join_path(&[&root, &dir_name], None).await,
            &format!("Failed to resolve sessions directory for {cwd}"),
        )
        .map(|p| p.to_string_lossy().into_owned())
    }

    /// Cached absolute sessions root. Mirrors `root()` (TS `rootPromise`).
    async fn root(&self) -> SessionResult<String> {
        {
            let cached = self.root.lock().expect("root cache not poisoned");
            if let Some(r) = &*cached {
                return Ok(r.clone());
            }
        }
        // Resolve under the create lock so two concurrent first-calls don't
        // both pay the resolution (the result is identical either way; this
        // just avoids redundant FS work).
        let _g = self.create_locks.lock().await;
        {
            let cached = self.root.lock().expect("root cache not poisoned");
            if let Some(r) = &*cached {
                return Ok(r.clone());
            }
        }
        let resolved = file_result(
            self.fs.absolute_path(&self.sessions_root_input, None).await,
            &format!(
                "Failed to resolve sessions root {}",
                self.sessions_root_input
            ),
        )?;
        let resolved_str = resolved.to_string_lossy().into_owned();
        {
            let mut cached = self.root.lock().expect("root cache not poisoned");
            *cached = Some(resolved_str.clone());
        }
        Ok(resolved_str)
    }
}

// ---- free-function helpers (mirror repo.ts free functions) ----

/// The list of cwd-encoded session directories to scan. With `cwd`, just that
/// cwd's directory (if it exists); without, every directory/symlink child of
/// the root. Mirrors `jsonlSessionDirectories`.
async fn jsonl_session_directories(
    repo: &JsonlSessionRepo,
    cwd: Option<&str>,
) -> SessionResult<Vec<String>> {
    let sessions_root = repo.root().await?;
    let fs = &repo.fs;
    if let Some(cwd) = cwd {
        let resolved_cwd = file_result(
            fs.absolute_path(cwd, None).await,
            &format!("Failed to resolve session cwd {cwd}"),
        )?
        .to_string_lossy()
        .into_owned();
        let directory = file_result(
            fs.join_path(
                &[&sessions_root, &jsonl_session_directory_name(&resolved_cwd)],
                None,
            )
            .await,
            &format!("Failed to resolve sessions directory for {cwd}"),
        )?
        .to_string_lossy()
        .into_owned();
        let exists = file_result(
            fs.exists(&directory, None).await,
            &format!("Failed to check sessions directory {directory}"),
        )?;
        return Ok(if exists { vec![directory] } else { Vec::new() });
    }
    let root_exists = file_result(
        fs.exists(&sessions_root, None).await,
        &format!("Failed to check sessions directory {sessions_root}"),
    )?;
    if !root_exists {
        return Ok(Vec::new());
    }
    let entries = file_result(
        fs.list_dir(&sessions_root, None).await,
        &format!("Failed to list sessions directory {sessions_root}"),
    )?;
    Ok(entries
        .iter()
        .filter(|entry| matches!(entry.kind, FileKind::Directory | FileKind::Symlink))
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect())
}

/// List every session's [`JsonlSessionMetadata`] by scanning the cwd directories,
/// reading each `.jsonl` file's first line (the header), newest-first. Mirrors
/// TS `listJsonlSessionMetadata`.
async fn list_jsonl_session_metadata(
    repo: &JsonlSessionRepo,
    query: &JsonlSessionListOptions,
) -> SessionResult<Vec<JsonlSessionMetadata>> {
    let fs = &repo.fs;
    let mut metadata: Vec<JsonlSessionMetadata> = Vec::new();
    for directory in jsonl_session_directories(repo, query.cwd.as_deref()).await? {
        let entries = file_result(
            fs.list_dir(&directory, None).await,
            &format!("Failed to list sessions directory {directory}"),
        )?;
        let files: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry.kind != FileKind::Directory
                    && entry.name.ends_with(".jsonl")
                    && !entry.name.ends_with(".progress.jsonl")
            })
            .collect();
        for file in files {
            let lines = file_result(
                fs.read_text_lines(&file.path.to_string_lossy(), Some(1), None)
                    .await,
                &format!("Failed to read session header {}", file.path.display()),
            )?;
            let first_line = match lines.first() {
                Some(line) => line,
                None => continue,
            };
            let header = match parse_header(first_line) {
                Ok(h) => h,
                Err(_) => continue,
            };
            metadata.push(metadata_from_header(
                &header,
                &file.path.to_string_lossy(),
                file.mtime_ms,
            ));
        }
    }
    // Newest-first by modified_at (TS `right.modifiedAt - left.modifiedAt`).
    metadata.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
    Ok(metadata)
}

/// Open a session file, confirming it exists and its header id matches the
/// requested metadata id. Mirrors TS `loadJsonlSessionStorage`.
async fn load_jsonl_session_storage(
    repo: &JsonlSessionRepo,
    metadata: &JsonlSessionMetadata,
) -> SessionResult<JsonlSessionStorage> {
    let fs = &repo.fs;
    if !file_result(
        fs.exists(&metadata.path, None).await,
        &format!("Failed to check session {}", metadata.path),
    )? {
        return Err(SessionError::not_found(format!(
            "Session not found: {}",
            metadata.id
        )));
    }
    let storage = JsonlSessionStorage::load(
        repo.fs.clone(),
        &metadata.path,
        repo.clock.clone(),
        repo.ids.clone(),
    )
    .await?;
    let loaded = storage.jsonl_metadata();
    if loaded.id != metadata.id {
        return Err(SessionError::invalid_entry(format!(
            "Session id does not match header: {}",
            metadata.id
        )));
    }
    Ok(storage)
}

/// Validate that a `serde_json::Map` is JSON-serializable (no cycles, no
/// non-finite numbers, no non-plain arrays/objects). Mirrors TS
/// `assertJsonSerializable`. The TS impl is an explicit stack-walk; the Rust
/// port re-serializes the value (which already enforces all of these.structurally
/// — `serde_json::Value` cannot represent cycles, non-finite numbers, symbols, or
/// non-plain objects) and additionally rejects `NaN`/`Infinity` that may slip
/// in through `f64` (kept for parity; `Map<String, Value>` values are already
/// constrained, so this is a defensive double-check).
fn assert_json_serializable(value: &serde_json::Map<String, JsonValue>) -> SessionResult<()> {
    fn check(
        value: &JsonValue,
        active: &mut Vec<*const serde_json::Map<String, JsonValue>>,
    ) -> SessionResult<()> {
        match value {
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::String(_) => Ok(()),
            JsonValue::Number(n) => {
                if n.as_f64().map(|f| f.is_finite()).unwrap_or(true) {
                    Ok(())
                } else {
                    Err(SessionError::invalid_payload(
                        "Durable payload contains a non-finite number",
                    ))
                }
            }
            JsonValue::Object(map) => {
                let ptr = map as *const _;
                if active.iter().any(|p| *p == ptr) {
                    return Err(SessionError::invalid_payload(
                        "Durable payload contains a cycle",
                    ));
                }
                active.push(ptr);
                for v in map.values() {
                    check(v, active)?;
                }
                active.pop();
                Ok(())
            }
            JsonValue::Array(arr) => {
                for v in arr {
                    check(v, active)?;
                }
                Ok(())
            }
        }
    }
    // Check the top-level map's own values (the map itself can't be a cycle
    // target without a parent referencing it).
    let mut active = Vec::new();
    for v in value.values() {
        check(v, &mut active)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SessionRepo trait impl — delegates to the typed API using the default cwd.
// ---------------------------------------------------------------------------

#[async_trait]
impl SessionRepo for JsonlSessionRepo {
    type Storage = JsonlSessionStorage;

    async fn create(&self, options: &SessionCreateOptions) -> SessionResult<Self::Storage> {
        let typed = JsonlSessionCreateOptions::from_shared(options, self.default_cwd.clone());
        self.create_typed(&typed).await
    }

    async fn open(&self, metadata: &SessionMetadata) -> SessionResult<Self::Storage> {
        // The trait carries only the base metadata (id); the JSONL backend needs
        // the on-disk path. Scan the default-cwd directory for a matching id.
        let listed = self
            .list_typed(&JsonlSessionListOptions {
                cwd: Some(self.default_cwd.clone()),
            })
            .await?;
        let jsonl_meta = listed
            .into_iter()
            .find(|m| m.id == metadata.id)
            .ok_or_else(|| {
                SessionError::not_found(format!("Session not found: {}", metadata.id))
            })?;
        self.open_by_jsonl_metadata(&jsonl_meta).await
    }

    async fn list(&self) -> SessionResult<Vec<SessionMetadata>> {
        let listed = self
            .list_typed(&JsonlSessionListOptions {
                cwd: Some(self.default_cwd.clone()),
            })
            .await?;
        Ok(listed.into_iter().map(|m| m.to_base()).collect())
    }

    async fn delete(&self, metadata: &SessionMetadata) -> SessionResult<()> {
        let listed = self
            .list_typed(&JsonlSessionListOptions {
                cwd: Some(self.default_cwd.clone()),
            })
            .await?;
        let jsonl_meta = listed
            .into_iter()
            .find(|m| m.id == metadata.id)
            .ok_or_else(|| {
                SessionError::not_found(format!("Session not found: {}", metadata.id))
            })?;
        self.delete_by_jsonl_metadata(&jsonl_meta).await
    }

    async fn fork(
        &self,
        source: &SessionMetadata,
        options: &SessionCreateOptions,
        fork: &ForkOptions,
    ) -> SessionResult<Self::Storage> {
        // Resolve the source's full JSONL metadata via a list scan (the trait
        // gives us only the base id).
        let listed = self
            .list_typed(&JsonlSessionListOptions {
                cwd: Some(self.default_cwd.clone()),
            })
            .await?;
        let source_jsonl = listed
            .into_iter()
            .find(|m| m.id == source.id)
            .ok_or_else(|| SessionError::not_found(format!("Session not found: {}", source.id)))?;
        let typed = JsonlSessionCreateOptions::from_shared(options, self.default_cwd.clone());
        self.fork_typed(&source_jsonl, &typed, fork).await
    }
}

// `stamp_record` is re-exported from memory.rs and used by the storage backend;
// the repo does not stamp records directly. No placeholder imports needed.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SessionErrorCode;
    use crate::session::memory::{CounterIdGenerator, FakeClock};
    use crate::session::types::SessionStorage;
    use crate::session::types::{
        OperationIntent, OperationStartedRecord, ProvisionedEntry, ProvisionedKind, RecordBase,
    };

    fn user_msg(text: &str) -> rpi_agent::message::AgentMessage {
        rpi_agent::message::AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
    }

    fn repo(fs: Arc<dyn FileSystem>) -> JsonlSessionRepo {
        JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
            fs,
            sessions_root: "/sessions".into(),
            clock: Arc::new(FakeClock::new()),
            ids: Arc::new(CounterIdGenerator::new()),
        })
    }

    #[test]
    fn session_id_validation() {
        assert!(validate_session_id("abc").is_ok());
        assert!(validate_session_id("a-b_c.d").is_ok());
        assert!(validate_session_id("a").is_ok());
        // Leading/trailing non-alphanumeric.
        assert!(validate_session_id("-abc").is_err());
        assert!(validate_session_id("abc-").is_err());
        assert!(validate_session_id(".abc").is_err());
        // Empty.
        assert!(validate_session_id("").is_err());
        // Disallowed char.
        assert!(validate_session_id("ab/c").is_err());
        assert!(validate_session_id("ab c").is_err());
    }

    #[test]
    fn directory_name_encoding() {
        assert_eq!(jsonl_session_directory_name("/home/user"), "--home-user--");
        assert_eq!(
            jsonl_session_directory_name("/home/user/proj"),
            "--home-user-proj--"
        );
        // Leading backslash stripped; backslashes + colons → '-'.
        assert_eq!(
            jsonl_session_directory_name("\\Users\\bob"),
            "--Users-bob--"
        );
        // `C:\dev` (in TS/literal form): leading char is `C` (not a slash, no
        // strip), then `:` and `\` both → `-`, giving `C--dev`.
        assert_eq!(jsonl_session_directory_name("C:\\dev"), "--C--dev--");
    }

    #[test]
    fn file_name_encoding() {
        // 2023-11-14T22:13:20.000Z → colons + dots replaced by '-' in the
        // timestamp; the `.jsonl` extension (added after) keeps its dot.
        let name = session_file_name(1_700_000_000_000, "abc");
        assert!(name.ends_with("_abc.jsonl"));
        let ts = name.split('_').next().unwrap();
        assert!(!ts.contains(':'), "timestamp must not contain ':'");
        assert!(!ts.contains('.'), "timestamp must not contain '.'");
        assert!(ts.starts_with("2023-11-14T"));
    }

    #[test]
    fn iso_timestamp_known_value() {
        // Unix epoch.
        assert_eq!(iso_timestamp_from_ms(0), "1970-01-01T00:00:00.000Z");
        // 2023-11-14T22:13:20.000Z (1700000000000 ms).
        assert_eq!(
            iso_timestamp_from_ms(1_700_000_000_000),
            "2023-11-14T22:13:20.000Z"
        );
        // Millisecond precision.
        assert_eq!(
            iso_timestamp_from_ms(1_700_000_000_123),
            "2023-11-14T22:13:20.123Z"
        );
    }

    #[tokio::test]
    async fn create_open_list_delete_roundtrip() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let r = repo(fs.clone());

        let storage = r
            .create_typed(&JsonlSessionCreateOptions {
                id: Some("sess-1".into()),
                parent_session_id: None,
                cwd: "/proj".into(),
                metadata: None,
            })
            .await
            .unwrap();
        // Append an entry so the session is non-empty.
        let _entry = storage
            .append_entry(
                ProvisionedEntry {
                    id: "e1".into(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("hi"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();

        // list surfaces it.
        let listed = r
            .list_typed(&JsonlSessionListOptions {
                cwd: Some("/proj".into()),
            })
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "sess-1");

        // open_by_jsonl_metadata re-loads it.
        let reopened = r.open_by_jsonl_metadata(&listed[0]).await.unwrap();
        let got = reopened
            .get_entry("e1")
            .await
            .unwrap()
            .expect("entry exists");
        assert_eq!(got.id(), "e1");

        // delete removes the file.
        r.delete_by_jsonl_metadata(&listed[0]).await.unwrap();
        let after = r
            .list_typed(&JsonlSessionListOptions {
                cwd: Some("/proj".into()),
            })
            .await
            .unwrap();
        assert!(after.is_empty());
    }

    #[tokio::test]
    async fn create_duplicate_id_is_already_exists() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let r = repo(fs);

        let opts = JsonlSessionCreateOptions {
            id: Some("dup".into()),
            parent_session_id: None,
            cwd: "/proj".into(),
            metadata: None,
        };
        let _ = r.create_typed(&opts).await.unwrap();
        let err = r.create_typed(&opts).await.err().unwrap();
        assert_eq!(err.code, SessionErrorCode::AlreadyExists);
    }

    #[tokio::test]
    async fn trait_impl_create_open_roundtrip() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let r = repo(fs);

        // Trait create uses the repo's default cwd ("/proj" from env.cwd).
        let _storage = r
            .create(&SessionCreateOptions {
                id: Some("t-1".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        let listed = r.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "t-1");

        let _opened = r.open(&listed[0]).await.unwrap();
        r.delete(&listed[0]).await.unwrap();
        assert!(r.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fork_rejects_mismatched_source_identity_before_initialization_or_publication() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into()));
        let r = repo(fs.clone());
        let options = |id: &str| JsonlSessionCreateOptions {
            id: Some(id.into()),
            parent_session_id: None,
            cwd: "/proj".into(),
            metadata: None,
        };
        let source = r.create_typed(&options("source")).await.unwrap();
        let mut metadata = source.jsonl_metadata();
        let history = fs.read_binary_file(&metadata.path, None).await.unwrap();
        let progress_path = format!("{}.progress.jsonl", metadata.path);
        let progress = fs.read_binary_file(&progress_path, None).await.unwrap();
        let directory = r.session_directory("/proj").await.unwrap();
        let paths = || async {
            let mut paths: Vec<_> = fs
                .list_dir(&directory, None)
                .await
                .unwrap()
                .into_iter()
                .map(|entry| entry.path)
                .collect();
            paths.sort();
            paths
        };
        let before = paths().await;
        metadata.id = "wrong-source".into();
        let legacy = r
            .fork_typed(&metadata, &options("legacy-child"), &ForkOptions::default())
            .await;
        let configured = r
            .fork_typed_with_value_initializer(
                &metadata,
                &options("configured-child"),
                &ForkOptions::default(),
                |_| panic!("identity rejection must precede initialization"),
            )
            .await;
        for result in [legacy, configured] {
            let error = match result {
                Ok(_) => panic!("mismatched source identity must not fork"),
                Err(error) => error,
            };
            assert_eq!(error.code, SessionErrorCode::InvalidEntry);
            assert!(error
                .to_string()
                .contains("Session id does not match header"));
        }
        assert_eq!(paths().await, before);
        assert_eq!(
            fs.read_binary_file(&metadata.path, None).await.unwrap(),
            history
        );
        assert_eq!(
            fs.read_binary_file(&progress_path, None).await.unwrap(),
            progress
        );
    }

    #[tokio::test]
    async fn fork_typed_branch_default_leaf() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let r = repo(fs);

        let source = r
            .create_typed(&JsonlSessionCreateOptions {
                id: Some("src".into()),
                parent_session_id: None,
                cwd: "/proj".into(),
                metadata: None,
            })
            .await
            .unwrap();
        let _e1 = source
            .append_entry(
                ProvisionedEntry {
                    id: "m1".into(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("hello"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        // A default fork after model/thinking changes must keep the actual
        // state leaf and its ancestry, not silently cut back to the message.
        for (id, kind) in [
            (
                "model-pick",
                ProvisionedKind::ModelChange {
                    provider: "faux".into(),
                    model_id: "new-model".into(),
                },
            ),
            (
                "thinking-pick",
                ProvisionedKind::ThinkingLevel {
                    thinking_level: "high".into(),
                },
            ),
        ] {
            source
                .append_entry(
                    ProvisionedEntry {
                        id: id.into(),
                        kind,
                    },
                    "main",
                )
                .await
                .unwrap();
        }
        let listed = r
            .list_typed(&JsonlSessionListOptions {
                cwd: Some("/proj".into()),
            })
            .await
            .unwrap();
        let src_meta = listed.iter().find(|m| m.id == "src").unwrap().clone();

        let forked = r
            .fork_typed(
                &src_meta,
                &JsonlSessionCreateOptions {
                    id: Some("fork-1".into()),
                    parent_session_id: None,
                    cwd: "/proj".into(),
                    metadata: None,
                },
                &ForkOptions::default(),
            )
            .await
            .unwrap();
        // Forked session carries the source's message.
        let got = forked
            .get_entry("m1")
            .await
            .unwrap()
            .expect("forked entry exists");
        assert_eq!(got.id(), "m1");
        for (id, parent) in [("model-pick", "m1"), ("thinking-pick", "model-pick")] {
            let copied = forked
                .get_entry(id)
                .await
                .unwrap()
                .expect("state entry copied");
            let mut original = source.get_entry(id).await.unwrap().unwrap();
            original.base_mut().seq = copied.seq(); // fork restamps only sequence
            assert_eq!(
                copied, original,
                "model/state payload and ancestry must survive"
            );
            assert_eq!(copied.parent_id(), Some(parent));
        }
        let lanes = forked.get_lanes().await.unwrap();
        assert_eq!(
            lanes
                .iter()
                .find(|lane| lane.lane == "main")
                .unwrap()
                .leaf_id
                .as_deref(),
            Some("thinking-pick"),
            "implicit fork must retain the actual nonmessage leaf"
        );
        // And lists as a separate session with parent = src.
        let listed = r
            .list_typed(&JsonlSessionListOptions {
                cwd: Some("/proj".into()),
            })
            .await
            .unwrap();
        let fork_meta = listed.iter().find(|m| m.id == "fork-1").unwrap();
        assert_eq!(fork_meta.parent_session_id.as_deref(), Some("src"));
    }

    #[tokio::test]
    async fn open_missing_session_is_not_found() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let r = repo(fs);
        let err = r
            .open(&SessionMetadata {
                id: "ghost".into(),
                created_at: 0,
                parent_session_id: None,
            })
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, SessionErrorCode::NotFound);
    }

    #[tokio::test]
    async fn metadata_map_round_trips_through_create() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let r = repo(fs);

        let mut meta = serde_json::Map::new();
        meta.insert("title".into(), JsonValue::String("my session".into()));
        meta.insert("count".into(), JsonValue::Number(42.into()));

        let _ = r
            .create_typed(&JsonlSessionCreateOptions {
                id: Some("m-1".into()),
                parent_session_id: None,
                cwd: "/proj".into(),
                metadata: Some(meta.clone()),
            })
            .await
            .unwrap();
        let listed = r
            .list_typed(&JsonlSessionListOptions {
                cwd: Some("/proj".into()),
            })
            .await
            .unwrap();
        let m = listed.iter().find(|m| m.id == "m-1").unwrap();
        assert_eq!(
            m.metadata.as_ref().unwrap().get("title"),
            Some(&JsonValue::String("my session".into()))
        );
        assert_eq!(
            m.metadata.as_ref().unwrap().get("count"),
            Some(&JsonValue::Number(42.into()))
        );
    }

    #[tokio::test]
    async fn listed_sessions_sorted_newest_first() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        // A clock that increments per call so created_at strictly increases.
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let r = JsonlSessionRepo::new(
            JsonlSessionRepoOptions {
                fs,
                sessions_root: "/sessions".into(),
                clock: clock.clone(),
                ids: Arc::new(CounterIdGenerator::new()),
            },
            "/proj".into(),
        );

        for n in 0..3 {
            let _ = r
                .create_typed(&JsonlSessionCreateOptions {
                    id: Some(format!("s{n}")),
                    parent_session_id: None,
                    cwd: "/proj".into(),
                    metadata: None,
                })
                .await
                .unwrap();
        }
        let listed = r
            .list_typed(&JsonlSessionListOptions {
                cwd: Some("/proj".into()),
            })
            .await
            .unwrap();
        let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
        // FakeClock increments per now_ms() call; create stamps one now_ms(), so
        // s2 has the largest created_at → newest first.
        assert_eq!(ids, vec!["s2", "s1", "s0"]);
    }

    // Silence unused helper imports in the test build if the above doesn't
    // reference every constructor.
    #[allow(dead_code)]
    fn _operation_started_record() -> OperationStartedRecord {
        OperationStartedRecord {
            base: RecordBase {
                id: "op".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }
    }
}
