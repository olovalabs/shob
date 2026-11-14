//! Nonexecuting canonical saved-session read. Intercepted before normal startup.
mod readonly_fs;
#[cfg(test)]
mod tests;
use rpi_harness::session::{
    jsonl::{storage::SavedSessionReadQuery, JsonlSessionStorage},
    memory::SystemClock,
    session::DefaultIdGenerator,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
pub(crate) fn capabilities() -> Value {
    json!({"versions":[1],"flag":"--resource-session-read","maxRequestBytes":MAX_REQUEST,"maxResponseBytes":MAX_RESPONSE,"maxReadBytes":64*1024*1024,"maxMessages":128,"maxMessageBytes":5*1024*1024,"maxMutations":65536,"maxProgressRecords":65536,"maxProgressFrames":65536,"maxLanes":32,"maxCustomTypes":8,"forbiddenCustomTypes":["rpi.value"],"maxCustomEntries":1024,"maxCustomBytes":2*1024*1024,"deadlineMs":10000,"nonexecuting":true,"noncreatingObservationLock":true,"strict":true})
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Cursor {
    version: u32,
    source: String,
    leaf_id: Option<String>,
    after_seq: u64,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    version: u32,
    session_id: String,
    session_root: PathBuf,
    session_path: PathBuf,
    limit: usize,
    custom_types: Vec<String>,
    cursor: Option<Cursor>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingFacts {
    execution_queue_or_recovery: bool,
    progress: bool,
    idle_proof_supported: bool,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedSessionRead {
    session_id: String,
    cwd: PathBuf,
    leaf_id: Option<String>,
    source: String,
    messages: Vec<Value>,
    cursor: Option<Cursor>,
    filtered: bool,
    custom_entries: Vec<rpi_harness::session::types::Entry>,
    binding: Value,
    pending: PendingFacts,
}
#[derive(Debug, Serialize)]
struct Failure {
    code: &'static str,
    message: String,
}
fn failure(message: impl Into<String>) -> Failure {
    let message = message.into();
    let code = if message.contains("limit exceeded") || message.contains("deadline exceeded") {
        "limit_exceeded"
    } else {
        "invalid_data"
    };
    Failure {
        code,
        message: message.chars().take(2048).collect(),
    }
}
struct SizeLimit(usize);
impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("saved read message byte limit exceeded"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[derive(Serialize)]
struct SuccessEnvelope<'a> {
    version: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    success: bool,
    data: &'a SavedSessionRead,
}
#[derive(Serialize)]
struct FailureEnvelope<'a> {
    version: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    success: bool,
    error: &'a Failure,
}
struct Output(Vec<u8>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) >= MAX_RESPONSE {
            return Err(std::io::Error::other("saved read response limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn request(path: &Path) -> Result<Request, Failure> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| failure(e.to_string()))?;
    if !meta.is_file() || meta.len() > MAX_REQUEST as u64 {
        return Err(failure(
            "saved read request limit exceeded or nonregular request",
        ));
    }
    let file = std::fs::File::open(path).map_err(|e| failure(e.to_string()))?;
    let mut bytes = Vec::new();
    file.take((MAX_REQUEST + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| failure(e.to_string()))?;
    if bytes.len() > MAX_REQUEST {
        return Err(failure("saved read request limit exceeded"));
    }
    let request: Request = serde_json::from_slice(&bytes)
        .map_err(|e| failure(format!("invalid saved read request: {e}")))?;
    if request.version != 1
        || request.session_id.is_empty()
        || request.session_id.len() > 512
        || request.session_id.chars().any(char::is_control)
        || !(1..=128).contains(&request.limit)
        || !request.session_root.is_absolute()
        || !request.session_path.is_absolute()
    {
        return Err(failure(
            "invalid saved read request identity/version/path/limit",
        ));
    }
    if request.cursor.as_ref().is_some_and(|c| {
        c.version != 1
            || c.after_seq == 0
            || c.source.len() != 64
            || !c
                .source
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || c.leaf_id
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 512 || s.chars().any(char::is_control))
    }) {
        return Err(failure("invalid saved read cursor"));
    }
    Ok(request)
}
async fn inspect(
    request: Request,
    deadline: Instant,
) -> Result<(SavedSessionRead, std::fs::File), Failure> {
    let root = std::fs::canonicalize(&request.session_root).map_err(|e| failure(e.to_string()))?;
    let path = std::fs::canonicalize(&request.session_path).map_err(|e| failure(e.to_string()))?;
    if !root.is_dir()
        || !path.starts_with(&root)
        || path == root
        || !std::fs::symlink_metadata(&request.session_path)
            .map_err(|e| failure(e.to_string()))?
            .is_file()
    {
        return Err(failure(
            "saved session escapes root or is not a regular file",
        ));
    }
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let meta = std::fs::symlink_metadata(&lock_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Failure {
                code: "unsupported",
                message:
                    "saved observation lock missing; deliberately open/close this session first"
                        .into(),
            }
        } else {
            failure(e.to_string())
        }
    })?;
    if !meta.is_file()
        || std::fs::canonicalize(&lock_path).map_err(|e| failure(e.to_string()))? != lock_path
    {
        return Err(failure("saved observation lock is aliased or nonregular"));
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .open(&lock_path)
        .map_err(|e| failure(e.to_string()))?;
    fs2::FileExt::try_lock_shared(&lock).map_err(|_| Failure {
        code: "busy",
        message: "saved session is owned by an active native writer".into(),
    })?;
    let fs = Arc::new(readonly_fs::ReadonlyFs::new(&path, deadline).map_err(failure)?);
    let data = JsonlSessionStorage::read_saved_session(
        fs.clone(),
        path.to_str()
            .ok_or_else(|| failure("saved path is not UTF-8"))?,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
        SavedSessionReadQuery {
            session_id: request.session_id.clone(),
            limit: request.limit,
            after_seq: request.cursor.as_ref().map(|c| c.after_seq),
            custom_types: request.custom_types,
            deadline,
        },
    )
    .await
    .map_err(|e| failure(e.to_string()))?;
    let source = fs.fingerprint();
    if request
        .cursor
        .as_ref()
        .is_some_and(|c| c.source != source || c.leaf_id != data.leaf_id)
    {
        return Err(Failure {
            code: "branch_changed",
            message: "saved session source/branch changed; restart paging".into(),
        });
    }
    if !data.cursor_valid {
        return Err(failure(
            "saved message cursor is not on the canonical branch",
        ));
    }
    let binding = crate::resource_selection::saved_binding_state(&data.values).map_err(failure)?;
    let cursor = if data.has_more {
        Some(Cursor {
            version: 1,
            source: source.clone(),
            leaf_id: data.leaf_id.clone(),
            after_seq: data.messages.last().unwrap().seq(),
        })
    } else {
        None
    };
    let mut size = SizeLimit(5 * 1024 * 1024);
    let messages = data
        .messages
        .into_iter()
        .map(|entry| {
            let rpi_harness::session::types::Entry::Message(entry) = entry else {
                return Err(failure("native message query returned a nonmessage"));
            };
            serde_json::to_writer(&mut size, &entry.message).map_err(|e| failure(e.to_string()))?;
            let mut value =
                serde_json::to_value(entry.message).map_err(|e| failure(e.to_string()))?;
            value["entryId"] = json!(entry.base.id);
            Ok(value)
        })
        .collect::<Result<Vec<_>, Failure>>()?;
    if Instant::now() >= deadline {
        return Err(failure("saved read processing deadline exceeded"));
    }
    Ok((
        SavedSessionRead {
            session_id: data.metadata.id,
            cwd: PathBuf::from(data.metadata.cwd),
            leaf_id: data.leaf_id,
            source,
            messages,
            cursor,
            filtered: true,
            custom_entries: data.custom_entries,
            binding,
            pending: PendingFacts {
                execution_queue_or_recovery: data.native_pending,
                progress: data.progress_pending,
                idle_proof_supported: data.idle_proof_supported,
            },
        },
        lock,
    ))
}
pub(crate) async fn run(argv: &[String]) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let result = if argv.len() != 2 || argv[0] != "--resource-session-read" {
        Err(Failure {
            code: "invalid_input",
            message:
                "--resource-session-read requires exactly one request file and no other arguments"
                    .into(),
        })
    } else {
        match request(Path::new(&argv[1])) {
            Ok(request) => inspect(request, deadline).await,
            Err(error) => Err(error),
        }
    };
    let mut output = Output(Vec::new());
    // The shared observation lock is retained through response construction.
    let (serialized, code) = match &result {
        Ok((data, _lock)) => (
            serde_json::to_writer(
                &mut output,
                &SuccessEnvelope {
                    version: 1,
                    kind: "saved_session_read",
                    success: true,
                    data,
                },
            ),
            0,
        ),
        Err(error) => (
            serde_json::to_writer(
                &mut output,
                &FailureEnvelope {
                    version: 1,
                    kind: "saved_session_read",
                    success: false,
                    error,
                },
            ),
            1,
        ),
    };
    if let Err(error) = serialized {
        println!(
            "{}",
            json!({"version":1,"type":"saved_session_read","success":false,"error":failure(error.to_string())})
        );
        return 1;
    }
    output.0.push(b'\n');
    match std::io::stdout().write_all(&output.0) {
        Ok(()) => code,
        Err(_) => 1,
    }
}
