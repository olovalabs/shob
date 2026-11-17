//! `InMemoryExecutionEnv` — an in-memory `ExecutionEnv` for tests. NOT a direct
//! mirror of a single TS file (the TS harness has no in-memory env; it tests
//! against `NodeExecutionEnv` + tempdirs). This is the Rust port's primary test
//! double, modeled to satisfy the same `FileSystem` + `Shell` contract so the
//! conformance suite can run identical assertions against both `InMemory` and
//! `Os`.
//!
//! Storage: `BTreeMap<String, InMemoryFile>` keyed by absolute path string.
//! `mtime_ms` advances on each mutation (synthetic — there's no real clock, so
//! we use a monotonic counter so order is stable + determinisitic-ish; the TS
//! uses real `Date.now()` but tests only check is_newer-than-prev).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::env::{
    check_cancel_exec, check_cancel_file, ExecutionEnv, FileContent, FileInfo, FileKind,
    FileSystem, Shell, ShellExecOptions, ShellOutput,
};
#[cfg(test)]
use crate::error::ExecutionErrorCode;
use crate::error::{io_to_file_code, ExecutionError, FileError, FileErrorCode};
use crate::file_mutation_queue::{MutatingEnv, MutationQueueRegistry};

/// An in-memory file: bytes + mtime counter.
#[derive(Clone)]
struct InMemoryFile {
    bytes: Vec<u8>,
    mtime_ms: i64,
    kind: FileKind,
}

#[derive(Default)]
struct Inner {
    cwd: PathBuf,
    files: BTreeMap<String, InMemoryFile>,
    /// Synthetic monotonic clock — every mutation bumps this so `mtime_ms`
    /// strictly increases within a process.
    clock: i64,
    /// Registered shell scripts: command-prefix → canned result. The `exec`
    /// impl looks up the longest matching prefix.
    shell_scripts: Vec<(String, ShellScript)>,
    /// Tracked "child pids" — simulated; `cleanup` clears them. Kept as a set
    /// of opaque i64 handles.
    active_pids: Vec<i64>,
}

/// A canned shell result for `InMemoryExecutionEnv::exec`.
#[derive(Clone)]
pub struct ShellScript {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// Optional delay before the result resolves (simulates a slow command for
    /// timeout/abort tests).
    pub delay_ms: Option<u64>,
}

impl ShellScript {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
            delay_ms: None,
        }
    }
    pub fn failure(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
            delay_ms: None,
        }
    }
}

/// In-memory `ExecutionEnv`. Clone shares the underlying state (cheap — the
/// `Arc<Mutex<Inner>>` is shared).
#[derive(Clone)]
pub struct InMemoryExecutionEnv {
    inner: Arc<Mutex<Inner>>,
    registry: Arc<MutationQueueRegistry>,
    /// Cwd snapshot, set at construction and never mutated. Stored outside the
    /// mutex so the sync `FileSystem::cwd()` can return `&Path` without locking.
    cwd_snapshot: Arc<PathBuf>,
}

impl InMemoryExecutionEnv {
    pub fn new() -> Self {
        Self::with_cwd(PathBuf::from("/"))
    }

    pub fn with_cwd(cwd: PathBuf) -> Self {
        let snap = Arc::new(cwd.clone());
        let mut files: BTreeMap<String, InMemoryFile> = BTreeMap::new();
        // The cwd (and every ancestor) always exists on a real fs — you're in
        // it. Register them as directories so tools that default their search
        // path to `.` (ls/find/grep) can `list_dir`/`exists` the cwd without the
        // test needing to `make_dir` it first. Matches `OsExecutionEnv` where
        // `cwd()` is guaranteed to resolve + be listable.
        let cwd_norm = Self::norm(&cwd.to_string_lossy());
        let mut acc = String::new();
        for seg in cwd_norm.trim_matches('/').split('/') {
            acc = if acc.is_empty() {
                format!("/{seg}")
            } else {
                format!("{acc}/{seg}")
            };
            files.insert(
                acc.clone(),
                InMemoryFile {
                    bytes: Vec::new(),
                    mtime_ms: 0,
                    kind: FileKind::Directory,
                },
            );
        }
        Self {
            inner: Arc::new(Mutex::new(Inner {
                cwd,
                files,
                clock: 0,
                shell_scripts: Vec::new(),
                active_pids: Vec::new(),
            })),
            registry: Arc::new(MutationQueueRegistry::new()),
            cwd_snapshot: snap,
        }
    }

    /// Register a canned shell result for commands whose start matches `prefix`.
    /// Longest-prefix match wins. Mirrors the faux-provider script pattern.
    pub async fn register_shell(&self, prefix: impl Into<String>, script: ShellScript) {
        let mut g = self.inner.lock().await;
        g.shell_scripts.push((prefix.into(), script));
        // Stable: longest-prefix-first lookup below.
        g.shell_scripts.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    /// Directly seed a file at an absolute path (test setup). Bumps mtime.
    pub async fn seed_file(&self, abs_path: &str, bytes: Vec<u8>) {
        let mut g = self.inner.lock().await;
        g.clock += 1;
        let mtime = g.clock;
        g.files.insert(
            abs_path.to_string(),
            InMemoryFile {
                bytes,
                mtime_ms: mtime,
                kind: FileKind::File,
            },
        );
    }

    async fn bump_clock(&self, inner: &mut Inner) -> i64 {
        inner.clock += 1;
        inner.clock
    }

    /// Resolve a path to an absolute `PathBuf` using the env's cwd.
    ///
    /// After joining, `.`/`..` components are collapsed component-by-component
    /// (without a FS round-trip) to match `OsExecutionEnv::normalize_absolute`.
    /// This matters because `PathBuf::join` preserves `.` as a `CurDir`
    /// component: `cwd.join(".")` yields `/tmp/work/.`, which would miss the
    /// `BTreeMap` key `/tmp/work` (and break tools that default their search
    /// path to `.`). Collapsing here keeps the in-memory env consistent with the
    /// OS env — every `absolute_path`/`resolve_key` consumer sees the same
    /// canonical form both in tests (`InMemory`) and in production (`Os`).
    fn resolve(cwd: &Path, path: &str) -> PathBuf {
        let pb = PathBuf::from(path);
        let joined = if pb.is_absolute() { pb } else { cwd.join(pb) };
        let mut out = PathBuf::new();
        for comp in joined.components() {
            match comp {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other),
            }
        }
        out
    }

    /// Normalize a path string to forward slashes. The internal `BTreeMap` keys
    /// are always stored normalized so that path-separator differences across
    /// platforms (Windows `\` vs POSIX `/`) do not break the prefix-based
    /// `list_dir` / `remove` lookups against POSIX-style test paths like
    /// `/sessions` / `/proj`. `PathBuf::push`/`join` insert the OS separator
    /// (`\` on Windows); without normalization, a file written at
    /// `/sessions\--proj--\file.jsonl` would never match a `list_dir` prefix of
    /// `/sessions/--proj--/` — which is exactly the cross-platform bug this
    /// guards. This is a test double, so canonicalizing to `/` is safe and keeps
    /// test paths portable.
    fn norm(s: &str) -> String {
        s.replace('\\', "/")
    }

    /// Resolve `path` against `cwd` and normalize to a forward-slash absolute
    /// string suitable for use as a `BTreeMap` key.
    fn resolve_key(cwd: &Path, path: &str) -> String {
        Self::norm(&Self::resolve(cwd, path).to_string_lossy())
    }
}

impl Default for InMemoryExecutionEnv {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FileSystem for InMemoryExecutionEnv {
    fn cwd(&self) -> &Path {
        &self.cwd_snapshot
    }

    async fn absolute_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let g = self.inner.lock().await;
        let resolved = Self::resolve(&g.cwd, path).to_string_lossy().into_owned();
        // Normalize to forward slashes so absolute_path() returns a string
        // consistent with the BTreeMap keys (POSIX-style), keeping the repo's
        // join_path + write_file + list_dir chain coherent on Windows.
        Ok(PathBuf::from(Self::norm(&resolved)))
    }

    async fn join_path(
        &self,
        parts: &[&str],
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, None)?;
        let mut out = String::new();
        for p in parts {
            if out.is_empty() {
                out = p.to_string();
            } else if out.ends_with('/') {
                out.push_str(p);
            } else {
                out.push('/');
                out.push_str(p);
            }
        }
        // Normalize any `\` the parts may carry to `/`.
        Ok(PathBuf::from(Self::norm(&out)))
    }

    async fn read_text_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        let bytes = self.read_binary_file(path, cancel).await?;
        String::from_utf8(bytes).map_err(|e| {
            FileError::new(FileErrorCode::Invalid, format!("invalid utf-8: {e}")).with_path(path)
        })
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError> {
        check_cancel_file(cancel, Some(path))?;
        if max_lines.map(|m| m == 0).unwrap_or(false) {
            return Ok(Vec::new());
        }
        let text = self.read_text_file(path, cancel).await?;
        let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        if text.ends_with('\n') {
            if lines.last().map(|s| s.is_empty()).unwrap_or(false) {
                lines.pop();
            }
        }
        if let Some(m) = max_lines {
            lines.truncate(m);
        }
        Ok(lines)
    }

    async fn read_binary_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        match g.files.get(&abs) {
            Some(f) if f.kind == FileKind::File => Ok(f.bytes.clone()),
            Some(f) if f.kind == FileKind::Directory => Err(FileError::new(
                FileErrorCode::IsDirectory,
                "path is a directory",
            )
            .with_path(&abs)),
            Some(f) if f.kind == FileKind::Symlink => {
                // Symlinks aren't really modeled here; treat as not-supported.
                Err(
                    FileError::new(FileErrorCode::NotSupported, "symlink read unsupported")
                        .with_path(&abs),
                )
            }
            _ => Err(FileError::not_found(&abs)),
        }
    }

    async fn write_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let mut g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        // Auto-create parent dirs (normalized so the key matches what later
        // reads/writes look up).
        let parent = Path::new(&abs)
            .parent()
            .map(|p| Self::norm(&p.to_string_lossy()));
        if let Some(p) = parent {
            if !p.is_empty() && !g.files.contains_key(&p) {
                let mtime = self.bump_clock(&mut g).await;
                g.files.insert(
                    p,
                    InMemoryFile {
                        bytes: Vec::new(),
                        mtime_ms: mtime,
                        kind: FileKind::Directory,
                    },
                );
            }
        }
        let mtime = self.bump_clock(&mut g).await;
        g.files.insert(
            abs.clone(),
            InMemoryFile {
                bytes: content.as_bytes().to_vec(),
                mtime_ms: mtime,
                kind: FileKind::File,
            },
        );
        Ok(())
    }

    async fn append_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let mut g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        let mtime = self.bump_clock(&mut g).await;
        match g.files.get_mut(&abs) {
            Some(f) if f.kind == FileKind::File => {
                f.bytes.extend_from_slice(content.as_bytes());
                f.mtime_ms = mtime;
                Ok(())
            }
            Some(f) if f.kind == FileKind::Directory => Err(FileError::new(
                FileErrorCode::IsDirectory,
                "path is a directory",
            )
            .with_path(&abs)),
            _ => {
                g.files.insert(
                    abs.clone(),
                    InMemoryFile {
                        bytes: content.as_bytes().to_vec(),
                        mtime_ms: mtime,
                        kind: FileKind::File,
                    },
                );
                Ok(())
            }
        }
    }

    async fn truncate_file(
        &self,
        path: &str,
        len: u64,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let mut g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        let mtime = self.bump_clock(&mut g).await;
        match g.files.get_mut(&abs) {
            Some(f) if f.kind == FileKind::File => {
                if len > f.bytes.len() as u64 {
                    return Err(FileError::new(
                        FileErrorCode::Invalid,
                        "truncation cannot grow a file",
                    )
                    .with_path(&abs));
                }
                f.bytes.truncate(len as usize);
                f.mtime_ms = mtime;
                Ok(())
            }
            Some(f) if f.kind == FileKind::Directory => Err(FileError::new(
                FileErrorCode::IsDirectory,
                "path is a directory",
            )
            .with_path(&abs)),
            _ => Err(FileError::new(FileErrorCode::NotFound, "path not found").with_path(&abs)),
        }
    }

    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(source))?;
        let mut g = self.inner.lock().await;
        let src_abs = Self::resolve_key(&g.cwd, source);
        let dst_abs = Self::resolve_key(&g.cwd, dest);
        match g.files.remove(&src_abs) {
            Some(f) => {
                let mtime = self.bump_clock(&mut g).await;
                g.files.insert(
                    dst_abs.clone(),
                    InMemoryFile {
                        bytes: f.bytes,
                        mtime_ms: mtime,
                        kind: f.kind,
                    },
                );
                Ok(())
            }
            None => Err(FileError::not_found(&src_abs)),
        }
    }

    async fn file_info(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        match g.files.get(&abs) {
            Some(f) => Ok(FileInfo {
                name: Path::new(&abs)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| abs.clone()),
                path: PathBuf::from(&abs),
                kind: f.kind,
                size: f.bytes.len() as u64,
                mtime_ms: f.mtime_ms,
            }),
            None => Err(FileError::not_found(&abs)),
        }
    }

    async fn list_dir(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        // Normalize the directory prefix for matching children.
        let prefix = if abs.ends_with('/') {
            abs.clone()
        } else {
            format!("{abs}/")
        };
        // Verify the dir itself exists.
        let dir_exists = g
            .files
            .get(&abs)
            .map(|f| f.kind == FileKind::Directory)
            .unwrap_or(abs == "/");
        if !dir_exists {
            return Err(if g.files.contains_key(&abs) {
                FileError::new(FileErrorCode::NotDirectory, "path is not a directory")
                    .with_path(&abs)
            } else {
                FileError::not_found(&abs)
            });
        }
        let mut out = Vec::new();
        for (k, f) in g.files.iter() {
            if let Some(rest) = k.strip_prefix(&prefix) {
                // Direct child: no further '/'.
                if !rest.is_empty() && !rest.contains('/') {
                    out.push(FileInfo {
                        name: rest.to_string(),
                        path: PathBuf::from(k),
                        kind: f.kind,
                        size: f.bytes.len() as u64,
                        mtime_ms: f.mtime_ms,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn canonical_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        if g.files.contains_key(&abs) || abs == "/" {
            Ok(PathBuf::from(&abs))
        } else {
            Err(FileError::not_found(&abs))
        }
    }

    async fn exists(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<bool, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        Ok(g.files.contains_key(&abs) || abs == "/")
    }

    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let mut g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        if recursive {
            // Create all ancestor dirs.
            let mut acc = String::new();
            for seg in abs.trim_matches('/').split('/') {
                if acc.is_empty() {
                    acc = format!("/{seg}");
                } else {
                    acc = format!("{acc}/{seg}");
                }
                if !g.files.contains_key(&acc) {
                    let mtime = self.bump_clock(&mut g).await;
                    g.files.insert(
                        acc.clone(),
                        InMemoryFile {
                            bytes: Vec::new(),
                            mtime_ms: mtime,
                            kind: FileKind::Directory,
                        },
                    );
                }
            }
        } else {
            if !g.files.contains_key(&abs) {
                let mtime = self.bump_clock(&mut g).await;
                g.files.insert(
                    abs.clone(),
                    InMemoryFile {
                        bytes: Vec::new(),
                        mtime_ms: mtime,
                        kind: FileKind::Directory,
                    },
                );
            }
        }
        Ok(())
    }

    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let mut g = self.inner.lock().await;
        let abs = Self::resolve_key(&g.cwd, path);
        if !g.files.contains_key(&abs) {
            if force {
                return Ok(());
            }
            return Err(FileError::not_found(&abs));
        }
        if recursive {
            // Remove this + all descendants.
            let prefix = if abs.ends_with('/') {
                abs.clone()
            } else {
                format!("{abs}/")
            };
            let to_remove: Vec<String> = g
                .files
                .keys()
                .filter(|k| *k == &abs || k.starts_with(&prefix))
                .cloned()
                .collect();
            for k in to_remove {
                g.files.remove(&k);
            }
        } else {
            // Non-recursive: error if dir has children.
            let prefix = if abs.ends_with('/') {
                abs.clone()
            } else {
                format!("{abs}/")
            };
            let has_children = g.files.keys().any(|k| k.starts_with(&prefix));
            if has_children {
                return Err(FileError::new(
                    FileErrorCode::NotSupported,
                    "directory not empty (use recursive)",
                )
                .with_path(&abs));
            }
            g.files.remove(&abs);
        }
        Ok(())
    }

    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, None)?;
        let mut g = self.inner.lock().await;
        let prefix = prefix.unwrap_or("tmp-");
        // Use the synthetic clock for a unique suffix.
        let n = g.clock + 1;
        let name = format!("{}{}", prefix, unique_suffix(n));
        let abs = format!("/tmp/{name}");
        let mtime = self.bump_clock(&mut g).await;
        g.files.insert(
            abs.clone(),
            InMemoryFile {
                bytes: Vec::new(),
                mtime_ms: mtime,
                kind: FileKind::Directory,
            },
        );
        Ok(PathBuf::from(abs))
    }

    async fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, None)?;
        let mut g = self.inner.lock().await;
        let n = g.clock + 1;
        let name = format!("{}{}{}", prefix, unique_suffix(n), suffix);
        let abs = format!("/tmp/{name}");
        let mtime = self.bump_clock(&mut g).await;
        g.files.insert(
            abs.clone(),
            InMemoryFile {
                bytes: Vec::new(),
                mtime_ms: mtime,
                kind: FileKind::File,
            },
        );
        Ok(PathBuf::from(abs))
    }

    async fn cleanup(&self) {
        let mut g = self.inner.lock().await;
        g.active_pids.clear();
    }
}

#[async_trait]
impl Shell for InMemoryExecutionEnv {
    async fn exec<'a>(
        &'a self,
        command: &str,
        mut options: ShellExecOptions<'a>,
    ) -> Result<ShellOutput, ExecutionError> {
        check_cancel_exec(options.cancel)?;
        let scripts = {
            let g = self.inner.lock().await;
            g.shell_scripts.clone()
        };
        // Longest-prefix match.
        let matched = scripts.iter().find(|(p, _)| command.starts_with(p));
        let script = match matched {
            Some((_, s)) => s.clone(),
            None => ShellScript {
                stdout: String::new(),
                stderr: format!("no shell script registered for: {command}"),
                exit_code: 127,
                delay_ms: None,
            },
        };

        // Simulate a child pid tracking.
        {
            let mut g = self.inner.lock().await;
            let pid = g.clock;
            g.active_pids.push(pid);
        }

        // Optional delay — honor cancellation during it.
        if let Some(ms) = script.delay_ms {
            let dur = std::time::Duration::from_millis(ms);
            if let Some(token) = options.cancel {
                tokio::select! {
                    _ = tokio::time::sleep(dur) => {}
                    _ = token.cancelled() => {
                        return Err(ExecutionError::aborted());
                    }
                }
            } else if let Some(secs) = options.timeout {
                let to = std::time::Duration::from_secs_f64(secs);
                let sleep = tokio::time::sleep(dur.min(to));
                tokio::pin!(sleep);
                let deadline = tokio::time::sleep(to);
                tokio::pin!(deadline);
                tokio::select! {
                    _ = &mut sleep => {}
                    _ = &mut deadline => {
                        return Err(ExecutionError::timeout(secs as u64));
                    }
                }
            } else {
                tokio::time::sleep(dur).await;
            }
        } else if let Some(secs) = options.timeout {
            // No delay but a timeout — simulate instant completion; the timeout
            // only matters if `delay_ms` exceeds it (handled above). If a test
            // wants timeout-without-delay it should register a script with
            // delay_ms > timeout.
            let _ = secs;
        }

        // Stream stdout/stderr to the callbacks (single chunk).
        if let Some(cb) = options.on_stdout.as_mut() {
            if !script.stdout.is_empty() {
                cb(&script.stdout);
            }
        }
        if let Some(cb) = options.on_stderr.as_mut() {
            if !script.stderr.is_empty() {
                cb(&script.stderr);
            }
        }
        if let Some(output) = &options.output {
            for (text, stderr) in [(&script.stdout, false), (&script.stderr, true)] {
                let mut rest = text.as_str();
                while !rest.is_empty() {
                    let mut end = rest.len().min(8192);
                    while !rest.is_char_boundary(end) {
                        end -= 1;
                    }
                    let chunk = rest[..end].to_string();
                    let chunk = if stderr {
                        crate::env::ShellOutputChunk::Stderr(chunk)
                    } else {
                        crate::env::ShellOutputChunk::Stdout(chunk)
                    };
                    tokio::select! {
                        _ = async {
                            if let Some(token) = options.cancel {
                                token.cancelled().await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        } => return Err(ExecutionError::aborted()),
                        _ = output.send(chunk) => {}
                    }
                    rest = &rest[end..];
                }
            }
        }

        Ok(ShellOutput {
            stdout: if options.capture_output {
                script.stdout
            } else {
                String::new()
            },
            stderr: if options.capture_output {
                script.stderr
            } else {
                String::new()
            },
            exit_code: script.exit_code,
        })
    }

    async fn cleanup(&self) {
        let mut g = self.inner.lock().await;
        g.active_pids.clear();
    }
}

#[async_trait]
impl ExecutionEnv for InMemoryExecutionEnv {}

#[async_trait]
impl MutatingEnv for InMemoryExecutionEnv {
    fn as_env(&self) -> &dyn ExecutionEnv {
        self
    }
    fn mutation_registry(&self) -> &MutationQueueRegistry {
        &self.registry
    }
}

/// Generate a deterministic-ish unique suffix from a counter. Mirrors
/// `mkdtemp`'s 6-char suffix shape loosely (test-only).
fn unique_suffix(n: i64) -> String {
    // Base36 of the counter, zero-padded to 6 chars.
    let mut s = String::new();
    let mut v = n.max(0) as u64;
    if v == 0 {
        return "000000".to_string();
    }
    while v > 0 {
        let d = (v % 36) as u8;
        let c = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
        s.insert(0, c as char);
        v /= 36;
    }
    while s.len() < 6 {
        s.insert(0, '0');
    }
    s
}

/// io classification helper re-exported for env consumers (e.g. `OsExecutionEnv`
/// test doubles). Kept here so the in-memory env can classify synthetic errors
/// without duplicating the table.
#[allow(dead_code)]
fn unused_io_reexport(_: std::io::Error) -> FileErrorCode {
    io_to_file_code(&std::io::Error::from(std::io::ErrorKind::Other))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_read_roundtrip() {
        let env = InMemoryExecutionEnv::new();
        env.write_file("/a/b.txt", "hello".into(), None)
            .await
            .unwrap();
        let text = env.read_text_file("/a/b.txt", None).await.unwrap();
        assert_eq!(text, "hello");
        // Parent dir auto-created.
        assert!(env.exists("/a", None).await.unwrap());
    }

    #[tokio::test]
    async fn rename_moves_bytes() {
        let env = InMemoryExecutionEnv::new();
        env.write_file("/x.txt", "data".into(), None).await.unwrap();
        env.rename_file("/x.txt", "/y.txt", None).await.unwrap();
        assert!(!env.exists("/x.txt", None).await.unwrap());
        assert_eq!(env.read_text_file("/y.txt", None).await.unwrap(), "data");
    }

    #[tokio::test]
    async fn list_dir_children() {
        let env = InMemoryExecutionEnv::new();
        env.create_dir("/d", true, None).await.unwrap();
        env.write_file("/d/a.txt", "1".into(), None).await.unwrap();
        env.write_file("/d/b.txt", "2".into(), None).await.unwrap();
        let entries = env.list_dir("/d", None).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
    }

    #[tokio::test]
    async fn remove_recursive() {
        let env = InMemoryExecutionEnv::new();
        env.create_dir("/r", true, None).await.unwrap();
        env.write_file("/r/a.txt", "1".into(), None).await.unwrap();
        env.remove("/r", true, true, None).await.unwrap();
        assert!(!env.exists("/r", None).await.unwrap());
        assert!(!env.exists("/r/a.txt", None).await.unwrap());
    }

    #[tokio::test]
    async fn temp_dir_and_file_are_unique() {
        let env = InMemoryExecutionEnv::new();
        let d1 = env.create_temp_dir(Some("prefix-"), None).await.unwrap();
        let d2 = env.create_temp_dir(Some("prefix-"), None).await.unwrap();
        assert_ne!(d1, d2);
        let f1 = env.create_temp_file("pre-", "-suf", None).await.unwrap();
        let f2 = env.create_temp_file("pre-", "-suf", None).await.unwrap();
        assert_ne!(f1, f2);
    }

    #[tokio::test]
    async fn shell_script_longest_prefix_match() {
        let env = InMemoryExecutionEnv::new();
        env.register_shell("git", ShellScript::success("git-out"))
            .await;
        env.register_shell("git status", ShellScript::success("clean"))
            .await;
        let out = env
            .exec("git status", ShellExecOptions::default())
            .await
            .unwrap();
        assert_eq!(out.stdout, "clean");
        let out2 = env
            .exec("git log", ShellExecOptions::default())
            .await
            .unwrap();
        assert_eq!(out2.stdout, "git-out");
    }

    #[tokio::test]
    async fn cancel_returns_aborted() {
        let env = InMemoryExecutionEnv::new();
        let token = CancellationToken::new();
        token.cancel();
        let r = env
            .exec(
                "anything",
                ShellExecOptions {
                    cancel: Some(&token),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(
            r,
            Err(ExecutionError {
                code: ExecutionErrorCode::Aborted,
                ..
            })
        ));
    }
}
