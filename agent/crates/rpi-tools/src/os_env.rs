//! Mirrors `packages/agent/src/harness/env/nodejs.ts` — `NodeExecutionEnv` →
//! `OsExecutionEnv`. Real-filesystem + real-process backend via `tokio::fs` +
//! `tokio::process`.
//!
//! Path resolution (\[`resolvePath`\]): tilde (`~`/`~/`, `~\` on Windows),
//! `file://` URL (parse failure silently keeps the literal string), absolute
//! → `canonicalize`-style normalize, relative → `cwd.join`. No env-var or
//! `~user` expansion. No symlink resolution (that's `canonical_path`).
//!
//! Shell detection (\[`getShellConfig`\]): explicit override → Windows
//! git-bash probe (`ProgramFiles\Git\bin\bash.exe`, `ProgramFiles(x86)\…`) →
//! `where bash.exe` → POSIX `/bin/bash` → `which bash` → `/bin/sh` fallback
//! (POSIX only). **No cmd.exe fallback** on Windows — failure is
//! `ExecutionError::ShellUnavailable`. Legacy WSL bash (`System32\bash.exe`)
//! uses `bash -s` stdin transport to avoid argv parse bugs.
//!
//! Kill tree: Windows terminates the shell's job object, falling back to
//! `taskkill /F /T /PID`; POSIX `kill -PG` (process group, since the child is
//! spawned as its own group leader) with fallback to `kill -PID`. Best-effort,
//! errors swallowed.
//!
//! `exec` abort+timeout bracketing (plan §5.4): the spawn is `tokio::select!`-ed
//! against the cancellation token and `tokio::time::timeout`. Resolution
//! priority: callbackError → timeout → aborted → exit-code-from-success.
//! `cleanup()` kills tracked child pids best-effort.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::env::{
    check_cancel_exec, check_cancel_file, ExecutionEnv, FileContent, FileInfo, FileKind,
    FileSystem, Shell, ShellExecOptions, ShellOutput, ShellOutputChunk,
};
use crate::error::{
    io_to_file_error, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode,
};
use crate::file_mutation_queue::{MutatingEnv, MutationQueueRegistry};

/// Max timeout in ms (Int32 max). Mirrors `MAX_TIMEOUT_MS`.
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
/// Max timeout in seconds. Mirrors `MAX_TIMEOUT_SECONDS`.
const MAX_TIMEOUT_SECONDS: f64 = 2_147_483.647;
/// Grace period after child exit for trailing stdio. Mirrors
/// `EXIT_STDIO_GRACE_MS`.
const EXIT_STDIO_GRACE_MS: u64 = 100;
/// After grace, drain at most this many immediately ready bytes per stream.
/// This preserves finite pipe backlog under spool backpressure without letting
/// continuously-writing background descendants keep execution open forever.
const EXIT_STDIO_READY_BYTES: usize = 1024 * 1024;

/// How the command is fed to the shell. Mirrors `commandTransport`.
#[derive(Clone, Copy)]
enum CommandTransport {
    /// Append the command as the last argv element (`bash -c <cmd>`).
    Argv,
    /// Pipe the command to the shell's stdin (`bash -s`).
    Stdin,
}

/// Resolved shell config. Mirrors `ShellConfig`.
struct ShellConfig {
    shell: PathBuf,
    args: Vec<String>,
    transport: CommandTransport,
}

mod append;
mod read;

/// Environment variables the **host** sets for its own runtime plumbing, which
/// must not be handed to a shell it spawns.
///
/// A spawned shell is a child process, so anything inherited reaches whatever
/// that shell runs — including another `rpi`. `RPI_SESSION_ID` names the session
/// *this process* serves, which is meaningless (and actively misleading) in a
/// descendant: the child would either report the parent's session or, seeing the
/// variable already present, refuse to publish its own. Both are wrong answers,
/// and neither is visible from the child.
///
/// This is only the host's own half. An *extension* can publish variables with
/// the same hazard, and it declares them itself; [`is_host_private_env`] is the
/// function that answers the full question. Use it rather than reading this
/// slice, or an extension's channel will be missed — which is precisely the bug
/// that let `LANGFUSE_PI_PARENT_*` through.
pub const HOST_PRIVATE_ENV: &[&str] = &[rpi_plugin_sdk::SESSION_ID_ENV];

/// Whether `name` must not be inherited by a spawned shell.
///
/// Two sources, because the variables come from two places:
///
/// - [`HOST_PRIVATE_ENV`] — the host's own plumbing.
/// - [`rpi_plugin_sdk::SUBAGENT_PARENT_ENV`] — the family an *extension*
///   declares for subagent nesting. An inherited copy makes a plain `rpi` started
///   from inside `bash` adopt its ancestor's trace, hang its root off the
///   ancestor's root span, and call itself `Subagent Turn` instead of opening its
///   own trace. A launcher that sets these on the child *it* starts is the
///   supported nesting path, so this is about not *inheriting* them, not about
///   forbidding the names.
///
/// The extension family is consulted **as the SDK's slice**, never as names
/// restated here. Restating them is what the previous version did, and it is
/// how the channel re-leaked when the plugin renamed them: the list went on
/// matching a name nobody set. Going through the constant means a rename is a
/// compile error, and a newly added family member is excluded automatically.
pub fn is_host_private_env(name: &str) -> bool {
    HOST_PRIVATE_ENV.contains(&name) || rpi_plugin_sdk::SUBAGENT_PARENT_ENV.contains(&name)
}

/// Real-OS `ExecutionEnv`. Clone shares the tracked-pid set + registry.
pub struct OsExecutionEnv {
    cwd: PathBuf,
    shell_path: Option<PathBuf>,
    shell_env: Option<HashMap<String, String>>,
    active_pids: Arc<Mutex<HashSet<u32>>>,
    registry: Arc<MutationQueueRegistry>,
}

impl OsExecutionEnv {
    pub fn new() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            shell_path: None,
            shell_env: None,
            active_pids: Arc::new(Mutex::new(HashSet::new())),
            registry: Arc::new(MutationQueueRegistry::new()),
        }
    }

    pub fn with_cwd(cwd: PathBuf) -> Self {
        Self {
            cwd,
            shell_path: None,
            shell_env: None,
            active_pids: Arc::new(Mutex::new(HashSet::new())),
            registry: Arc::new(MutationQueueRegistry::new()),
        }
    }

    pub fn with_shell_path(mut self, shell_path: PathBuf) -> Self {
        self.shell_path = Some(shell_path);
        self
    }

    pub fn with_shell_env(mut self, env: HashMap<String, String>) -> Self {
        self.shell_env = Some(env);
        self
    }

    /// Resolve a path (tilde, file://, absolute/relative) against `cwd`. Pure.
    fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
        let mut p = path.to_string();
        // Tilde expansion (literal only).
        if p == "~" {
            p = home_dir_string();
        } else if let Some(rest) = p.strip_prefix("~/") {
            p = format!("{}/{}", home_dir_string(), rest);
        } else if cfg!(windows) {
            if let Some(rest) = p.strip_prefix("~\\") {
                p = format!("{}\\{}", home_dir_string(), rest);
            }
        }
        // file:// URL → path. Parse failure silently keeps the literal.
        if p.starts_with("file://") {
            if let Some(local) = file_url_to_path(&p) {
                p = local;
            }
        }
        let pb = PathBuf::from(&p);
        if pb.is_absolute() {
            normalize_absolute(&pb)
        } else {
            normalize_absolute(&cwd.join(pb))
        }
    }

    /// Detect + return the shell config, or `ShellUnavailable`. Mirrors
    /// `getShellConfig`.
    async fn get_shell_config(&self) -> Result<ShellConfig, ExecutionError> {
        // Spawn remains authoritative; avoid blocking metadata on the runtime.
        if let Some(sp) = &self.shell_path {
            if tokio::fs::metadata(sp).await.is_ok_and(|m| m.is_file()) {
                return Ok(bash_shell_config(sp.clone()));
            }
            return Err(ExecutionError::new(
                ExecutionErrorCode::ShellUnavailable,
                format!("Custom shell path not found: {}", sp.display()),
            ));
        }
        // Windows probe.
        #[cfg(windows)]
        {
            if let Some(cfg) = windows_probe_bash().await {
                return Ok(cfg);
            }
            return Err(ExecutionError::new(
                ExecutionErrorCode::ShellUnavailable,
                "bash not found. Install Git for Windows / MSYS2 / Cygwin, or set shellPath.",
            ));
        }
        // POSIX.
        #[cfg(not(windows))]
        {
            if tokio::fs::metadata("/bin/bash")
                .await
                .is_ok_and(|m| m.is_file())
            {
                return Ok(ShellConfig {
                    shell: PathBuf::from("/bin/bash"),
                    args: vec!["-c".to_string()],
                    transport: CommandTransport::Argv,
                });
            }
            // `which bash` fallback.
            if let Some(found) =
                run_command("which", &["bash"], std::time::Duration::from_millis(5000)).await
            {
                let trimmed = found.trim();
                if !trimmed.is_empty()
                    && tokio::fs::metadata(trimmed)
                        .await
                        .is_ok_and(|m| m.is_file())
                {
                    return Ok(ShellConfig {
                        shell: PathBuf::from(trimmed),
                        args: vec!["-c".to_string()],
                        transport: CommandTransport::Argv,
                    });
                }
            }
            // sh fallback.
            return Ok(ShellConfig {
                shell: PathBuf::from("sh"),
                args: vec!["-c".to_string()],
                transport: CommandTransport::Argv,
            });
        }
    }
}

impl Default for OsExecutionEnv {
    fn default() -> Self {
        Self::new()
    }
}

fn home_dir_string() -> String {
    dirs::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn normalize_absolute(p: &Path) -> PathBuf {
    // std::path doesn't expose ".."/"." collapse without canonicalize (which
    // hits the FS). Rebuild component-by-component.
    let mut out = PathBuf::new();
    for comp in p.components() {
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

fn file_url_to_path(url: &str) -> Option<String> {
    // Minimal file:// → path conversion. Handles `file:///abs/path` and
    // Windows `file:///C:/...`. Parse failure → None (caller keeps literal).
    let rest = url.strip_prefix("file://")?;
    if let Some(drive_rest) = rest.strip_prefix('/') {
        // `/C:/...` (Windows drive) or `/abs/path`.
        if drive_rest.len() >= 2 && drive_rest.as_bytes()[1] == b':' {
            return Some(drive_rest.to_string());
        }
        return Some(format!("/{}", drive_rest));
    }
    Some(rest.to_string())
}

fn bash_shell_config(shell: PathBuf) -> ShellConfig {
    if is_legacy_wsl_bash_path(&shell) {
        ShellConfig {
            shell,
            args: vec!["-s".to_string()],
            transport: CommandTransport::Stdin,
        }
    } else {
        ShellConfig {
            shell,
            args: vec!["-c".to_string()],
            transport: CommandTransport::Argv,
        }
    }
}

fn is_legacy_wsl_bash_path(p: &Path) -> bool {
    let s = p.to_string_lossy().replace('/', "\\").to_lowercase();
    s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && matches!(
            s.get(1..),
            Some(":\\windows\\system32\\bash.exe" | ":\\windows\\sysnative\\bash.exe")
        )
}

#[cfg(windows)]
async fn windows_probe_bash() -> Option<ShellConfig> {
    let pf = std::env::var("ProgramFiles").ok();
    let pf86 = std::env::var("ProgramFiles(x86)").ok();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = pf {
        candidates.push(PathBuf::from(p).join("Git").join("bin").join("bash.exe"));
    }
    if let Some(p) = pf86 {
        candidates.push(PathBuf::from(p).join("Git").join("bin").join("bash.exe"));
    }
    for c in &candidates {
        if tokio::fs::metadata(c).await.is_ok() {
            return Some(bash_shell_config(c.clone()));
        }
    }
    // `where bash.exe`.
    if let Some(found) = run_command(
        "where",
        &["bash.exe"],
        std::time::Duration::from_millis(5000),
    )
    .await
    {
        let first = found.lines().next().unwrap_or("").trim().to_string();
        if !first.is_empty() && tokio::fs::metadata(&first).await.is_ok() {
            return Some(bash_shell_config(PathBuf::from(first)));
        }
    }
    None
}

/// Run a quick command, capture stdout, with a timeout. Mirrors `runCommand`
/// (used only by shell detection). Returns `None` on spawn failure / timeout /
/// empty stdout.
async fn run_command(cmd: &str, args: &[&str], timeout: std::time::Duration) -> Option<String> {
    let mut command = tokio::process::Command::new(cmd);
    command.args(args);
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::null());
    command.stdin(std::process::Stdio::null());
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    {
        // Hide the console window for the probe.
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let child = command.spawn().ok()?;
    let mut guard = ChildGroupGuard {
        pid: child.id(),
        active_pids: None,
        armed: true,
    };
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        _ => return None,
    };
    if !out.status.success() {
        return None;
    }
    // `where` prints paths in the console code page (e.g. a Chinese user name).
    let s = crate::output_decode::decode_output_async(&out.stdout)
        .await
        .ok()?;
    if s.trim().is_empty() {
        None
    } else {
        guard.armed = false;
        Some(s)
    }
}

/// One job object per running shell, by root pid. `taskkill /T` walks parent
/// pids, and an MSYS shell's children are not on that chain: `sleep` under Git
/// Bash outlives it. Job membership is inherited by every descendant.
///
/// Kill-on-close is armed only when killing, so whatever a command leaves
/// running on purpose keeps running after it ends normally.
#[cfg(windows)]
mod shell_jobs {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    use win32job::{ExtendedLimitInfo, Job};

    static JOBS: LazyLock<Mutex<HashMap<u32, Job>>> = LazyLock::new(Default::default);

    /// Put a shell spawned with `CREATE_SUSPENDED` in its own job, then let it
    /// run: no descendant can start outside the job. A shell that gets no job
    /// still runs, and cancelling it falls back to `taskkill`.
    pub fn start(pid: u32, process: std::os::windows::io::RawHandle) -> Result<(), String> {
        if let Ok(job) = Job::create() {
            if job.assign_process(process as isize).is_ok() {
                JOBS.lock().unwrap().insert(pid, job);
            }
        }
        rpi_plugin_sdk::process::resume_child(pid)
    }

    /// End every process in `pid`'s job: arm kill-on-close, then drop the only
    /// handle. False when `pid` has no job or arming failed.
    pub fn terminate(pid: u32) -> bool {
        let Some(job) = JOBS.lock().unwrap().remove(&pid) else {
            return false;
        };
        let mut info = ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        job.set_extended_limit_info(&info).is_ok()
    }

    pub fn forget(pid: u32) {
        JOBS.lock().unwrap().remove(&pid);
    }
}

/// Kill a process tree. Windows: the shell's job object, else
/// `taskkill /F /T /PID`. POSIX: `kill -PG` with fallback to `kill -PID`.
/// Best-effort, errors swallowed.
async fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        if shell_jobs::terminate(pid) {
            return;
        }
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        let mut cmd = tokio::process::Command::new("taskkill");
        cmd.args(&["/F", "/T", "/PID", &pid.to_string()]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        // Wait for taskkill itself to finish. Spawning it fire-and-forget lets
        // the shell's `child.wait()` race the tree termination, and a listener
        // can keep its inherited stdout/stderr handles open long enough to
        // deadlock the capture join after Ctrl+C.
        let _ = cmd.status().await;
    }
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        // Process group: negative pid.
        let pg = Pid::from_raw(-(pid as i32));
        if kill(pg, Signal::SIGKILL).is_err() {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
    }
}

/// Kill only on interruption: successful commands may deliberately leave daemons.
struct ChildGroupGuard {
    pid: Option<u32>,
    active_pids: Option<Arc<Mutex<HashSet<u32>>>>,
    armed: bool,
}

impl Drop for ChildGroupGuard {
    fn drop(&mut self) {
        let Some(pid) = self.pid else { return };
        if let Some(pids) = &self.active_pids {
            pids.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
        }
        if self.armed {
            #[cfg(unix)]
            {
                use nix::sys::signal::{kill, Signal};
                use nix::unistd::Pid;
                let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
            }
            #[cfg(windows)]
            if !shell_jobs::terminate(pid) {
                // The fallback needs an async wait, but job termination above
                // runs synchronously even while the execution future is dropped.
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(kill_process_tree(pid));
                }
            }
        } else {
            #[cfg(windows)]
            shell_jobs::forget(pid);
        }
    }
}

#[async_trait]
impl FileSystem for OsExecutionEnv {
    async fn open_append_file(
        &self,
        path: &str,
    ) -> Result<Option<Box<dyn crate::env::AppendFile>>, FileError> {
        let path = Self::resolve_path(&self.cwd, path);
        Ok(Some(Box::new(append::OsAppendFile::open(path).await?)))
    }

    async fn open_read_file(
        &self,
        path: &str,
    ) -> Result<Option<Arc<dyn crate::env::ReadFile>>, FileError> {
        let path = Self::resolve_path(&self.cwd, path);
        Ok(Some(Arc::new(read::OsReadFile::open(path).await?)))
    }

    async fn truncate_file(
        &self,
        path: &str,
        len: u64,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        let error_path = abs.to_string_lossy().into_owned();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new().write(true).open(&abs)?;
            if len > file.metadata()?.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "truncation cannot grow a file",
                ));
            }
            file.set_len(len)
        })
        .await
        .map_err(|error| {
            FileError::new(
                FileErrorCode::Unknown,
                format!("file truncation task failed: {error}"),
            )
        })?
        .map_err(|error| io_to_file_error(error, Some(&error_path)))
    }

    fn cwd(&self) -> &Path {
        &self.cwd
    }

    async fn absolute_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, Some(path))?;
        Ok(Self::resolve_path(&self.cwd, path))
    }

    async fn join_path(
        &self,
        parts: &[&str],
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, None)?;
        let mut out = PathBuf::new();
        for p in parts {
            out.push(p);
        }
        Ok(out)
    }

    async fn read_text_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        let bytes = read_cancellable(&abs, cancel).await?;
        String::from_utf8(bytes).map_err(|e| {
            FileError::new(FileErrorCode::Invalid, format!("invalid utf-8: {e}"))
                .with_path(abs.to_string_lossy())
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
        let abs = Self::resolve_path(&self.cwd, path);
        read::read_text_lines(&abs, max_lines, cancel).await
    }

    async fn read_binary_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        read_cancellable(&abs, cancel).await
    }

    async fn write_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        // mkdir parent (recursive).
        if let Some(parent) = abs.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return Err(io_to_file_error(e, Some(&abs.to_string_lossy())));
                }
            }
        }
        check_cancel_file(cancel, Some(&abs.to_string_lossy()))?;
        write_cancellable(&abs, content.as_bytes(), cancel).await
    }

    async fn append_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        if let Some(parent) = abs.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
        }
        use tokio::io::AsyncWriteExt;
        let res = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&abs)
            .await
        {
            Ok(mut f) => {
                // Tokio may acknowledge a buffered write before the blocking
                // filesystem operation completes. Await it before returning so
                // an immediate read/metadata call observes the appended bytes.
                match f.write_all(content.as_bytes()).await {
                    Ok(()) => f.flush().await,
                    Err(error) => Err(error),
                }
            }
            Err(e) => return Err(io_to_file_error(e, Some(&abs.to_string_lossy()))),
        };
        res.map_err(|e| io_to_file_error(e, Some(&abs.to_string_lossy())))
    }

    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(source))?;
        let src = Self::resolve_path(&self.cwd, source);
        let dst = Self::resolve_path(&self.cwd, dest);
        tokio::fs::rename(&src, &dst)
            .await
            .map_err(|e| io_to_file_error(e, Some(&src.to_string_lossy())))
    }

    async fn file_info(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        let meta = tokio::fs::symlink_metadata(&abs)
            .await
            .map_err(|e| io_to_file_error(e, Some(&abs.to_string_lossy())))?;
        let ft = meta.file_type();
        let kind = if ft.is_file() {
            FileKind::File
        } else if ft.is_dir() {
            FileKind::Directory
        } else if ft.is_symlink() {
            FileKind::Symlink
        } else {
            return Err(
                FileError::new(FileErrorCode::Invalid, "Unsupported file type")
                    .with_path(abs.to_string_lossy()),
            );
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(FileInfo {
            name: abs
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| abs.to_string_lossy().into_owned()),
            path: abs.clone(),
            kind,
            size: meta.len(),
            mtime_ms,
        })
    }

    async fn list_dir(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        let mut entries = tokio::fs::read_dir(&abs)
            .await
            .map_err(|e| io_to_file_error(e, Some(&abs.to_string_lossy())))?;
        let mut out = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| io_to_file_error(e, Some(&abs.to_string_lossy())))?
        {
            if let Some(t) = cancel {
                if t.is_cancelled() {
                    return Err(FileError::aborted().with_path(abs.to_string_lossy()));
                }
            }
            let ep = entry.path();
            let meta = match tokio::fs::symlink_metadata(&ep).await {
                Ok(m) => m,
                Err(e) => {
                    // Skip unsupported types silently; propagate real stat errors.
                    let code = crate::error::io_to_file_code(&e);
                    if code == FileErrorCode::NotSupported {
                        continue;
                    }
                    return Err(io_to_file_error(e, Some(&ep.to_string_lossy())));
                }
            };
            let ft = meta.file_type();
            let kind = if ft.is_file() {
                FileKind::File
            } else if ft.is_dir() {
                FileKind::Directory
            } else if ft.is_symlink() {
                FileKind::Symlink
            } else {
                continue; // silently skip sockets/fifos/etc.
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.push(FileInfo {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: ep,
                kind,
                size: meta.len(),
                mtime_ms,
            });
        }
        Ok(out)
    }

    async fn search_contents(
        &self,
        path: &str,
        search: crate::search::ContentSearch,
        cancel: Option<&CancellationToken>,
    ) -> Result<crate::search::ContentSearchResult, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        let cancel = cancel.cloned();
        tokio::task::spawn_blocking(move || {
            crate::search::search_native(&abs, search, cancel.as_ref())
        })
        .await
        .map_err(|error| io_to_file_error(std::io::Error::other(error), Some(path)))?
    }

    async fn canonical_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        tokio::fs::canonicalize(&abs)
            .await
            .map_err(|e| io_to_file_error(e, Some(&abs.to_string_lossy())))
    }

    async fn exists(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<bool, FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        match tokio::fs::symlink_metadata(&abs).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_to_file_error(e, Some(&abs.to_string_lossy()))),
        }
    }

    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        let res = if recursive {
            tokio::fs::create_dir_all(&abs).await
        } else {
            tokio::fs::create_dir(&abs).await
        };
        res.map_err(|e| io_to_file_error(e, Some(&abs.to_string_lossy())))
    }

    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        check_cancel_file(cancel, Some(path))?;
        let abs = Self::resolve_path(&self.cwd, path);
        let res = if recursive {
            tokio::fs::remove_dir_all(&abs).await
        } else {
            // Try file first, then empty dir.
            match tokio::fs::remove_file(&abs).await {
                Ok(()) => Ok(()),
                Err(_) => tokio::fs::remove_dir(&abs).await,
            }
        };
        match res {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && force => Ok(()),
            Err(e) => Err(io_to_file_error(e, Some(&abs.to_string_lossy()))),
        }
    }

    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, None)?;
        let prefix = prefix.unwrap_or("tmp-");
        // Use tempfile's NamedTempFile-less builder for a dir.
        let tmp =
            tempfile::tempdir_in(std::env::temp_dir()).map_err(|e| io_to_file_error(e, None))?;
        // tempfile picks a random name; rename to include the prefix.
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp); // keep the dir alive; caller manages cleanup.
                               // Build a prefixed sibling.
        let final_path = std::env::temp_dir().join(format!("{}{}", prefix, random_suffix()));
        // Rename the tempdir into the prefixed path.
        if let Err(e) = tokio::fs::rename(&path, &final_path).await {
            // Fallback: just return the original random path.
            let _ = e;
            return Ok(path);
        }
        Ok(final_path)
    }

    async fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        check_cancel_file(cancel, None)?;
        // Create an isolated temp dir (matching the TS pattern of one dir per
        // file), then write an empty file named `<prefix><random><suffix>`.
        let dir = self.create_temp_dir(Some("tmp-"), cancel).await?;
        let name = format!("{}{}{}", prefix, random_suffix(), suffix);
        let path = dir.join(name);
        tokio::fs::File::create(&path)
            .await
            .map_err(|e| io_to_file_error(e, Some(&path.to_string_lossy())))?;
        Ok(path)
    }

    async fn cleanup(&self) {
        let pids: Vec<u32> = self
            .active_pids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect();
        for pid in pids {
            kill_process_tree(pid).await;
        }
        self.active_pids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// Read a file to bytes, honoring cooperative cancellation.
async fn read_cancellable(
    path: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<Vec<u8>, FileError> {
    let read = tokio::fs::read(path);
    let res = match cancel {
        Some(t) => tokio::select! {
            r = read => r,
            _ = t.cancelled() => return Err(FileError::aborted().with_path(path.to_string_lossy())),
        },
        None => read.await,
    };
    res.map_err(|e| io_to_file_error(e, Some(&path.to_string_lossy())))
}

/// Write bytes to a file (truncate), honoring cooperative cancellation.
async fn write_cancellable(
    path: &Path,
    bytes: &[u8],
    cancel: Option<&CancellationToken>,
) -> Result<(), FileError> {
    use tokio::io::AsyncWriteExt;
    let f = tokio::fs::File::create(path)
        .await
        .map_err(|e| io_to_file_error(e, Some(&path.to_string_lossy())))?;
    let write = async {
        let mut f = f;
        f.write_all(bytes).await?;
        f.flush().await?;
        Ok::<(), std::io::Error>(())
    };
    let res = match cancel {
        Some(t) => tokio::select! {
            r = write => r,
            _ = t.cancelled() => return Err(FileError::aborted().with_path(path.to_string_lossy())),
        },
        None => write.await,
    };
    res.map_err(|e| io_to_file_error(e, Some(&path.to_string_lossy())))
}

fn random_suffix() -> String {
    // Short random suffix via uuid v4 (no Time—it's a UUID, not a timestamp).
    uuid::Uuid::new_v4().simple().to_string()
}

#[async_trait]
impl Shell for OsExecutionEnv {
    async fn exec<'a>(
        &'a self,
        command: &str,
        options: ShellExecOptions<'a>,
    ) -> Result<ShellOutput, ExecutionError> {
        // Pre-flight abort.
        check_cancel_exec(options.cancel)?;
        // Resolve timeout.
        let timeout = match resolve_timeout(options.timeout) {
            Ok(t) => t,
            Err(e) => return Err(e),
        };
        // Resolve cwd.
        let cwd = match &options.cwd {
            Some(c) => Self::resolve_path(&self.cwd, &c.to_string_lossy()),
            None => self.cwd.clone(),
        };
        // Shell config. An explicit override is used by native shell tools
        // such as PowerShell; otherwise preserve the default Bash behavior.
        let shell_cfg = match &options.shell {
            Some(override_shell) => ShellConfig {
                shell: override_shell.program.clone(),
                args: override_shell.args.clone(),
                transport: CommandTransport::Argv,
            },
            None => self.get_shell_config().await?,
        };

        // Build the spawn.
        let mut cmd = tokio::process::Command::new(&shell_cfg.shell);
        cmd.current_dir(&cwd);
        // Environment: inherit + shell_env + per-call. The host's own private
        // plumbing (`HOST_PRIVATE_ENV`) is skipped: it describes *this* process,
        // not the shell or anything the shell runs, and inheriting it makes a
        // nested `rpi` misreport its session. A caller that genuinely wants a
        // value passed down can still set it explicitly through `env`/`shell_env`
        // below (those are applied after this loop and win).
        if options.inherit_env {
            // Command already inherits the environment. Remove only private
            // host keys instead of copying every inherited key/value twice.
            for key in HOST_PRIVATE_ENV
                .iter()
                .chain(rpi_plugin_sdk::SUBAGENT_PARENT_ENV.iter())
            {
                cmd.env_remove(*key);
            }
        } else {
            cmd.env_clear();
        }
        if let Some(base) = &self.shell_env {
            for (k, v) in base {
                cmd.env(k, v);
            }
        }
        if let Some(extra) = &options.env {
            for (k, v) in extra {
                cmd.env(k, v);
            }
        }
        // stdio.
        cmd.kill_on_drop(true);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0); // child becomes its own group leader.
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
            // Suspended until it is in its job (see `shell_jobs::start`).
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        }

        // Args: stdin transport → `bash -s` (no command argv); argv → append.
        let transport = shell_cfg.transport;
        cmd.args(&shell_cfg.args);
        if matches!(transport, CommandTransport::Argv) {
            cmd.arg(command);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::SpawnError,
                    format!("failed to spawn shell: {e}"),
                ));
            }
        };
        let pid = child.id();
        let mut group_guard = ChildGroupGuard {
            pid,
            active_pids: Some(self.active_pids.clone()),
            armed: true,
        };
        #[cfg(windows)]
        {
            let started = match (pid, child.raw_handle()) {
                (Some(pid), Some(process)) => shell_jobs::start(pid, process),
                _ => Err("the shell has no process handle".to_owned()),
            };
            if let Err(error) = started {
                if let Some(pid) = pid {
                    shell_jobs::forget(pid);
                }
                let _ = child.start_kill();
                return Err(ExecutionError::new(
                    ExecutionErrorCode::SpawnError,
                    format!("failed to start shell: {error}"),
                ));
            }
        }
        if let Some(pid) = pid {
            self.active_pids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pid);
        }

        // Drive legacy stdin concurrently with output, status and deadlines.
        // Argv transport closes stdin immediately so prompts observe EOF.
        let stdin = child.stdin.take();
        let stdin_task = async move {
            if let Some(mut stdin) = stdin {
                if matches!(transport, CommandTransport::Stdin) {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(command.as_bytes()).await;
                }
            }
        };

        // Collect stdout/stderr via separate read tasks, firing callbacks.
        let mut stdout_buf = String::new();
        let mut stderr_buf = String::new();
        // Streaming consumers (the bash capture layer) already keep the output
        // they need, so do not retain a second unbounded copy here.
        let capture_output = options.capture_output;
        let cancel = options.cancel;
        let stop_readers = CancellationToken::new();
        let stop_execution = CancellationToken::new();
        let mut on_stdout = options.on_stdout;
        let mut on_stderr = options.on_stderr;
        let stdout_output = options.output.clone();
        let stderr_output = options.output;

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();

        let stdout_task = async {
            if let Some(s) = stdout.as_mut() {
                let mut buf = [0u8; 8192];
                let mut decoder = crate::output_decode::StreamDecoder::new();
                use tokio::io::AsyncReadExt;
                let mut post_grace_bytes = 0;
                loop {
                    if stop_readers.is_cancelled() && post_grace_bytes >= EXIT_STDIO_READY_BYTES {
                        break;
                    }
                    let read_result = tokio::select! {
                        biased;
                        _ = stop_execution.cancelled() => break,
                        _ = async {
                            match cancel {
                                Some(token) => token.cancelled().await,
                                None => std::future::pending::<()>().await,
                            }
                        } => break,
                        // Once grace expires, consume only immediately ready
                        // bytes. Slow spool backpressure is not descendant EOF.
                        result = s.read(&mut buf) => result,
                        _ = stop_readers.cancelled() => break,
                    };
                    let n = match read_result {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    if stop_readers.is_cancelled() {
                        post_grace_bytes += n;
                    }
                    let chunk = decoder.push_async(&buf[..n]).await?;
                    if chunk.is_empty() {
                        continue;
                    }
                    if let Some(cb) = on_stdout.as_mut() {
                        // Callback errors are swallowed (TS sets callbackError;
                        // v1 simplifies — the callback contract is best-effort).
                        cb(&chunk);
                    }
                    if capture_output {
                        stdout_buf.push_str(&chunk);
                    }
                    // Read before admission: an idle stderr stream must not
                    // reserve the only slot needed by stdout (or vice versa).
                    if let Some(output) = &stdout_output {
                        tokio::select! {
                            biased;
                            _ = stop_execution.cancelled() => break,
                            _ = async { if let Some(token) = cancel { token.cancelled().await } else { std::future::pending::<()>().await } } => break,
                            _ = output.send(ShellOutputChunk::Stdout(chunk)) => {},
                        }
                    }
                }
                let tail = decoder.finish_async().await?;
                if !tail.is_empty() {
                    if let Some(cb) = on_stdout.as_mut() {
                        cb(&tail);
                    }
                    if capture_output {
                        stdout_buf.push_str(&tail);
                    }
                    if let Some(output) = &stdout_output {
                        tokio::select! {
                            biased;
                            _ = stop_execution.cancelled() => {},
                            _ = async { if let Some(token) = cancel { token.cancelled().await } else { std::future::pending::<()>().await } } => {},
                            _ = output.send(ShellOutputChunk::Stdout(tail)) => {},
                        }
                    }
                }
            }
            Ok::<(), tokio::task::JoinError>(())
        };
        let stderr_task = async {
            if let Some(s) = stderr.as_mut() {
                let mut buf = [0u8; 8192];
                let mut decoder = crate::output_decode::StreamDecoder::new();
                use tokio::io::AsyncReadExt;
                let mut post_grace_bytes = 0;
                loop {
                    if stop_readers.is_cancelled() && post_grace_bytes >= EXIT_STDIO_READY_BYTES {
                        break;
                    }
                    let read_result = tokio::select! {
                        biased;
                        _ = stop_execution.cancelled() => break,
                        _ = async {
                            match cancel {
                                Some(token) => token.cancelled().await,
                                None => std::future::pending::<()>().await,
                            }
                        } => break,
                        result = s.read(&mut buf) => result,
                        _ = stop_readers.cancelled() => break,
                    };
                    let n = match read_result {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    if stop_readers.is_cancelled() {
                        post_grace_bytes += n;
                    }
                    let chunk = decoder.push_async(&buf[..n]).await?;
                    if chunk.is_empty() {
                        continue;
                    }
                    if let Some(cb) = on_stderr.as_mut() {
                        cb(&chunk);
                    }
                    if capture_output {
                        stderr_buf.push_str(&chunk);
                    }
                    if let Some(output) = &stderr_output {
                        tokio::select! {
                            biased;
                            _ = stop_execution.cancelled() => break,
                            _ = async { if let Some(token) = cancel { token.cancelled().await } else { std::future::pending::<()>().await } } => break,
                            _ = output.send(ShellOutputChunk::Stderr(chunk)) => {},
                        }
                    }
                }
                let tail = decoder.finish_async().await?;
                if !tail.is_empty() {
                    if let Some(cb) = on_stderr.as_mut() {
                        cb(&tail);
                    }
                    if capture_output {
                        stderr_buf.push_str(&tail);
                    }
                    if let Some(output) = &stderr_output {
                        tokio::select! {
                            biased;
                            _ = stop_execution.cancelled() => {},
                            _ = async { if let Some(token) = cancel { token.cancelled().await } else { std::future::pending::<()>().await } } => {},
                            _ = output.send(ShellOutputChunk::Stderr(tail)) => {},
                        }
                    }
                }
            }
            Ok::<(), tokio::task::JoinError>(())
        };

        // Wait for the child, with timeout + cancellation.
        let mut timed_out = false;
        let mut tree_killed = false;
        let mut output_error = None;
        let exit_status = {
            // We pin stdout/stderr take handles + the wait future, then drive
            // them concurrently inside a single select! block. `child` is only
            // touched again AFTER the join completes (kill + reap), so the
            // borrow ends cleanly.
            let status_fut = async {
                tokio::select! {
                    status = child.wait() => return status,
                    _ = async {
                        match timeout {
                            Some(duration) => tokio::time::sleep(duration).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => timed_out = true,
                    _ = async {
                        match cancel {
                            Some(token) => token.cancelled().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {},
                    _ = stop_execution.cancelled() => {},
                }
                stop_execution.cancel();
                if let Some(pid) = child.id() {
                    kill_process_tree(pid).await;
                    tree_killed = true;
                }
                child.wait().await
            };
            // Run stdio + status concurrently — but the CHILD is the driver:
            // once it is reaped the command is over, and the call has to end even
            // if something else still holds the stdout/stderr pipes open.
            //
            // This is the "`npm run` 卡住" shape. A script that starts a watcher,
            // a dev-server or a background daemon leaves a grandchild running with
            // the inherited pipes, so reading to EOF waits for a process the
            // command no longer owns. No timeout rescues that: the timeout wraps
            // `child.wait()` and the child already exited successfully.
            //
            // The pipes are still read while the child runs: a command that writes
            // more than the pipe buffer (64 KiB on Linux) blocks until someone
            // reads, so waiting for the child first would deadlock on any broad
            // `rg` or build log.
            tokio::pin!(status_fut, stdout_task, stderr_task, stdin_task);
            let (mut stdout_done, mut stderr_done, mut stdin_done) = (false, false, false);
            let status_res = loop {
                tokio::select! {
                    status = &mut status_fut => break status,
                    result = &mut stdout_task, if !stdout_done => {
                        stdout_done = true;
                        if let Err(error) = result {
                            output_error = Some(error);
                            stop_execution.cancel();
                        }
                    },
                    result = &mut stderr_task, if !stderr_done => {
                        stderr_done = true;
                        if let Err(error) = result {
                            output_error = Some(error);
                            stop_execution.cancel();
                        }
                    },
                    () = &mut stdin_task, if !stdin_done => stdin_done = true,
                }
            };
            // Drain what is already in the pipe — a completed write is already in
            // the pipe buffer, so this is not a race — then stop, without waiting
            // for an EOF that may never come. A command whose own process is gone
            // should not be able to hold the call open.
            let drain = async {
                tokio::join!(
                    async {
                        if stdout_done {
                            Ok(())
                        } else {
                            (&mut stdout_task).await
                        }
                    },
                    async {
                        if stderr_done {
                            Ok(())
                        } else {
                            (&mut stderr_task).await
                        }
                    },
                )
            };
            tokio::pin!(drain);
            let results = match tokio::time::timeout(
                std::time::Duration::from_millis(EXIT_STDIO_GRACE_MS),
                &mut drain,
            )
            .await
            {
                Ok(results) => results,
                Err(_) => {
                    // Stop idle reads, but finish already-read decoder tails.
                    stop_readers.cancel();
                    drain.await
                }
            };
            for result in [results.0, results.1] {
                if let Err(error) = result {
                    output_error = Some(error);
                }
            }
            status_res
        };

        // A cancellation can race successful reaping (including during drain).
        // In that case the guard must still terminate surviving descendants.
        group_guard.armed = !tree_killed
            && (exit_status.is_err()
                || output_error.is_some()
                || cancel.map(|token| token.is_cancelled()).unwrap_or(false));
        if let Some(error) = output_error {
            return Err(ExecutionError::new(
                ExecutionErrorCode::Unknown,
                format!("shell output decoding failed: {error}"),
            ));
        }

        // Resolution priority: timedOut → cancelled → status.
        let status = match exit_status {
            Ok(s) => s,
            Err(e) => {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::SpawnError,
                    format!("child wait failed: {e}"),
                ));
            }
        };
        if timed_out {
            return Err(ExecutionError::timeout(
                options.timeout.map(|f| f as u64).unwrap_or(0),
            ));
        }
        if cancel.map(|t| t.is_cancelled()).unwrap_or(false) {
            return Err(ExecutionError::aborted());
        }
        let exit_code = status.code().unwrap_or(0);
        Ok(ShellOutput {
            stdout: stdout_buf,
            stderr: stderr_buf,
            exit_code,
        })
    }

    async fn cleanup(&self) {
        FileSystem::cleanup(self).await
    }
}

#[async_trait]
impl ExecutionEnv for OsExecutionEnv {}

#[async_trait]
impl MutatingEnv for OsExecutionEnv {
    fn as_env(&self) -> &dyn ExecutionEnv {
        self
    }
    fn mutation_registry(&self) -> &MutationQueueRegistry {
        &self.registry
    }
}

/// Validate a timeout (seconds). Mirrors `resolveTimeoutMs` semantics.
fn resolve_timeout(timeout: Option<f64>) -> Result<Option<std::time::Duration>, ExecutionError> {
    match timeout {
        None => Ok(None),
        Some(secs) => {
            if !secs.is_finite() || secs <= 0.0 {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    "Invalid timeout: must be a finite number of seconds",
                ));
            }
            let ms = secs * 1000.0;
            if ms > MAX_TIMEOUT_MS as f64 {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    format!(
                        "Invalid timeout: maximum is {} seconds",
                        MAX_TIMEOUT_SECONDS
                    ),
                ));
            }
            Ok(Some(std::time::Duration::from_secs_f64(secs)))
        }
    }
}
