//! File-based IPC for the ignored process-test child. Never opens session storage.

use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use uuid::Uuid;

const MANIFEST_ENV: &str = "RPI_CFG_TEST_IPC_MANIFEST";
const NONCE_ENV: &str = "RPI_CFG_TEST_IPC_NONCE";
const CHILD_TEST: &str = "session::experimental_native::storage::process_tests::child";
const CAP: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const REAP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

struct Channel {
    root: PathBuf,
    dir: PathBuf,
    nonce: String,
    pid: u32,
}

impl Channel {
    fn diagnostic(&self, message: &str) -> String {
        // A combined maximum of 8 KiB of raw log bytes, even for large logs.
        format!(
            "IPC: {message}; PID={}; root={}; role_dir={}\nstdout tail:\n{}\nstderr tail:\n{}",
            self.pid,
            self.root.display(),
            self.dir.display(),
            log_tail(&self.dir.join("stdout.log")),
            log_tail(&self.dir.join("stderr.log")),
        )
    }

    fn fail(&self, message: &str) -> ! {
        panic!("{}", self.diagnostic(message))
    }

    fn path(&self, kind: &str, seq: u64) -> PathBuf {
        self.dir.join(format!("{kind}-{seq}.json"))
    }

    fn publish(&self, kind: &str, seq: u64, stage: &str, value: Value) {
        let message = json!({
            "nonce": self.nonce,
            "seq": seq,
            "stage": stage,
            "value": value,
        });
        if let Err(error) = publish(&self.path(kind, seq), &message) {
            self.fail(&format!("publish {kind}-{seq}: {error}"));
        }
    }

    fn receive(&self, kind: &str, seq: u64) -> Option<(String, Value)> {
        let path = self.path(kind, seq);
        let message = match read_json(&path) {
            Ok(message) => message,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => self.fail(&format!("read {}: {error}", path.display())),
        };
        if message.get("nonce").and_then(Value::as_str) != Some(self.nonce.as_str())
            || message.get("seq").and_then(Value::as_u64) != Some(seq)
        {
            self.fail(&format!("invalid nonce/sequence in {}", path.display()));
        }
        let Some(stage) = message.get("stage").and_then(Value::as_str) else {
            self.fail(&format!("missing string stage in {}", path.display()));
        };
        let Some(value) = message.get("value") else {
            self.fail(&format!("missing value in {}", path.display()));
        };
        Some((stage.to_owned(), value.clone()))
    }
}

pub(super) struct ChildProcess {
    child: Child,
    channel: Channel,
    event_seq: u64,
    command_seq: u64,
    reaped: bool,
}

impl ChildProcess {
    pub(super) fn spawn(root: &Path, role: &str, payload: Value) -> Self {
        let mut channel = Channel {
            root: root.to_owned(),
            dir: root.join(role),
            nonce: Uuid::new_v4().to_string(),
            pid: std::process::id(),
        };
        // Each role owns a fresh directory, so stale sequence files cannot be reused.
        let mut components = Path::new(role).components();
        if !matches!(components.next(), Some(Component::Normal(_)))
            || components.next().is_some()
            || role.contains(['/', '\\', ':'])
        {
            channel.fail("role must be one nonempty directory component");
        }
        let prepare = (|| -> io::Result<(PathBuf, File, File)> {
            fs::create_dir_all(root)?;
            channel.root = fs::canonicalize(root)?;
            channel.dir = channel.root.join(role);
            fs::create_dir(&channel.dir)?;
            let stdout = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(channel.dir.join("stdout.log"))?;
            let stderr = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(channel.dir.join("stderr.log"))?;
            // The guard is independent of the manifest and must agree with the env nonce.
            publish(&channel.dir.join("nonce.guard"), &json!(channel.nonce))?;
            let manifest = channel.dir.join("manifest.json");
            publish(
                &manifest,
                &json!({
                    "nonce": channel.nonce,
                    "root": channel.root,
                    "directory": channel.dir,
                    "payload": payload,
                }),
            )?;
            Ok((manifest, stdout, stderr))
        })();
        let (manifest, stdout, stderr) = prepare.unwrap_or_else(|error| {
            channel.fail(&format!(
                "prepare child (PID is launcher until spawn): {error}"
            ))
        });
        let executable = env::current_exe()
            .unwrap_or_else(|error| channel.fail(&format!("current_exe: {error}")));
        let child = Command::new(executable)
            .args([
                "--exact",
                CHILD_TEST,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(MANIFEST_ENV, manifest)
            .env(NONCE_ENV, &channel.nonce)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap_or_else(|error| channel.fail(&format!("spawn child: {error}")));
        channel.pid = child.id();
        Self {
            child,
            channel,
            event_seq: 1,
            command_seq: 1,
            reaped: false,
        }
    }

    pub(super) fn event(&mut self, expected: &str) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            // Exit alone never establishes that the requested stage was reached.
            self.require_alive("waiting for event");
            if let Some((stage, value)) = self.channel.receive("events", self.event_seq) {
                if stage != expected {
                    self.channel.fail(&format!(
                        "event {}: expected {expected:?}, got {stage:?}",
                        self.event_seq
                    ));
                }
                self.event_seq += 1;
                return value;
            }
            if Instant::now() >= deadline {
                self.channel
                    .fail(&format!("timeout waiting for event {expected:?}"));
            }
            thread::sleep(POLL);
        }
    }

    pub(super) fn send(&mut self, command: &str) {
        self.require_alive("sending command");
        self.channel
            .publish("commands", self.command_seq, command, Value::Null);
        self.command_seq += 1;
    }

    pub(super) fn kill_and_reap(&mut self) {
        // In particular, do not mistake a naturally exited child for the kill point.
        self.require_alive("before explicit kill");
        if let Err(error) = self.child.kill() {
            self.channel.fail(&format!("kill: {error}"));
        }
        if let Err(error) = self.bounded_reap(REAP_TIMEOUT) {
            self.channel.fail(&error);
        }
    }

    pub(super) fn finish(&mut self) {
        if self.reaped {
            self.channel
                .fail("finish called on an already reaped child");
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    if !status.success() {
                        self.channel.fail(&format!("child finished with {status}"));
                    }
                    return;
                }
                Ok(None) => {}
                Err(error) => self.channel.fail(&format!("finish try_wait: {error}")),
            }
            if Instant::now() >= deadline {
                self.channel
                    .fail("timeout waiting for successful child exit");
            }
            thread::sleep(POLL);
        }
    }

    fn require_alive(&mut self, context: &str) {
        if self.reaped {
            self.channel
                .fail(&format!("{context}: child already reaped"));
        }
        match self.child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                self.reaped = true;
                self.channel
                    .fail(&format!("{context}: premature child exit: {status}"));
            }
            Err(error) => self.channel.fail(&format!("{context}: try_wait: {error}")),
        }
    }

    fn bounded_reap(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.reaped = true;
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => return Err(format!("cleanup try_wait: {error}")),
            }
            if Instant::now() >= deadline {
                return Err("cleanup timeout reaping killed child".to_owned());
            }
            thread::sleep(POLL);
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let mut errors = Vec::new();
        match self.child.try_wait() {
            Ok(Some(_)) => self.reaped = true,
            Ok(None) => {
                if let Err(error) = self.child.kill() {
                    errors.push(format!("cleanup kill: {error}"));
                }
            }
            Err(error) => {
                errors.push(format!("cleanup initial try_wait: {error}"));
                // Only this owned Child is ever signalled; no PID lookup or tree kill.
                if let Err(error) = self.child.kill() {
                    errors.push(format!("cleanup kill: {error}"));
                }
            }
        }
        if !self.reaped {
            if let Err(error) = self.bounded_reap(REAP_TIMEOUT) {
                errors.push(error);
            }
        }
        if !errors.is_empty() {
            let message = self.channel.diagnostic(&errors.join("; "));
            if thread::panicking() {
                // Avoid both a second panic and eprintln!'s fallible stderr write.
                let _ = writeln!(io::stderr().lock(), "{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

pub(super) struct Peer {
    channel: Channel,
    event_seq: u64,
    command_seq: u64,
}

impl Peer {
    pub(super) fn from_env() -> Option<(Self, Value)> {
        let manifest = PathBuf::from(env::var_os(MANIFEST_ENV)?);
        let mut channel = Channel {
            root: manifest
                .parent()
                .unwrap_or(Path::new("<invalid>"))
                .to_owned(),
            dir: manifest
                .parent()
                .unwrap_or(Path::new("<invalid>"))
                .to_owned(),
            nonce: String::new(),
            pid: std::process::id(),
        };
        // Validate location and the independent guard before accepting any payload.
        if !manifest.is_absolute()
            || manifest.file_name() != Some(OsStr::new("manifest.json"))
            || manifest
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            channel.fail("manifest must be an absolute manifest.json path without '..'");
        }
        let parent = fs::canonicalize(&channel.dir)
            .unwrap_or_else(|error| channel.fail(&format!("manifest parent: {error}")));
        if parent != channel.dir {
            channel.fail("manifest parent must be canonical (no symlink/relative alias)");
        }
        let checked_manifest = fs::canonicalize(&manifest)
            .unwrap_or_else(|error| channel.fail(&format!("manifest path: {error}")));
        if checked_manifest != manifest {
            channel.fail("manifest must not be a symlink or path alias");
        }
        channel.nonce = env::var(NONCE_ENV)
            .unwrap_or_else(|error| channel.fail(&format!("missing/invalid nonce env: {error}")));
        if Uuid::parse_str(&channel.nonce).is_err() {
            channel.fail("nonce is not a UUID");
        }
        let guard_path = parent.join("nonce.guard");
        if fs::canonicalize(&guard_path)
            .unwrap_or_else(|error| channel.fail(&format!("nonce guard path: {error}")))
            != guard_path
        {
            channel.fail("nonce guard must not be a symlink or path alias");
        }
        let guard = read_json(&guard_path)
            .unwrap_or_else(|error| channel.fail(&format!("read nonce guard: {error}")));
        if guard.as_str() != Some(channel.nonce.as_str()) {
            channel.fail("nonce guard mismatch");
        }
        let data = read_json(&manifest)
            .unwrap_or_else(|error| channel.fail(&format!("read manifest: {error}")));
        if data.get("nonce").and_then(Value::as_str) != Some(channel.nonce.as_str()) {
            channel.fail("manifest nonce mismatch");
        }
        let directory: PathBuf =
            serde_json::from_value(data.get("directory").cloned().unwrap_or(Value::Null))
                .unwrap_or_else(|error| channel.fail(&format!("manifest directory: {error}")));
        let root: PathBuf =
            serde_json::from_value(data.get("root").cloned().unwrap_or(Value::Null))
                .unwrap_or_else(|error| channel.fail(&format!("manifest root: {error}")));
        if directory != parent || !root.is_absolute() || directory.parent() != Some(root.as_path())
        {
            channel.fail("manifest directory/root do not match the validated parent");
        }
        channel.root = root;
        let payload = data
            .get("payload")
            .cloned()
            .unwrap_or_else(|| channel.fail("manifest missing payload"));
        Some((
            Self {
                channel,
                event_seq: 1,
                command_seq: 1,
            },
            payload,
        ))
    }

    pub(super) fn validate_artifact(&self, path: &Path) {
        let canonical = fs::canonicalize(path)
            .unwrap_or_else(|error| self.channel.fail(&format!("artifact path: {error}")));
        let file = fs::symlink_metadata(path)
            .unwrap_or_else(|error| self.channel.fail(&format!("artifact metadata: {error}")));
        if !path.is_absolute()
            || canonical.parent() != Some(self.channel.root.as_path())
            || path.file_name() != Some(OsStr::new("fixture.rpinative"))
            || !file.file_type().is_file()
            || file.file_type().is_symlink()
        {
            self.channel
                .fail("artifact must be the fixture file directly within the guarded temp root");
        }
    }

    pub(super) fn emit(&mut self, stage: &str, value: Value) {
        self.channel.publish("events", self.event_seq, stage, value);
        self.event_seq += 1;
    }

    pub(super) fn command(&mut self) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some((command, value)) = self.channel.receive("commands", self.command_seq) {
                if !value.is_null() {
                    self.channel.fail("command value must be null");
                }
                self.command_seq += 1;
                return command;
            }
            if Instant::now() >= deadline {
                // Never synthesize a release command on timeout.
                self.channel.fail("timeout waiting for parent command");
            }
            thread::sleep(POLL);
        }
    }
}

fn read_json(path: &Path) -> io::Result<Value> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take((CAP + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > CAP {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC file exceeds 64 KiB",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn publish(path: &Path, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() > CAP {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC message exceeds 64 KiB",
        ));
    }
    if path.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "IPC destination already exists",
        ));
    }
    let temporary = path.with_file_name(format!(".ipc-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.flush()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn log_tail(path: &Path) -> String {
    let result = (|| -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(4096)))?;
        let mut bytes = Vec::new();
        file.take(4096).read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    match result {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("<log unavailable: {error}>"),
    }
}
