//! Append-only agent conversation history.
//!
//! Each pane (and each selected model within it) owns a JSONL file under
//! `<data>/conversations/<pane-uid>/`. Appending raw events rather than a
//! rendered transcript keeps writes cheap during streaming, and replaying the
//! events through the same reducer (`push_chat_event`) rebuilds exactly what
//! the UI showed before the restart.

use crate::agent_auth::{AgentModel, ModelBackend};
use crate::agent_bridge::AgentEvent;
use crate::paths;
use crate::{ChatMessage, ChatRole, ToolState, push_chat_event, task_list_update};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 64 * 1024;
const MAX_QUEUED_ENTRIES: usize = 256;
const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
const MAX_OPEN_FILES: usize = 64;

struct QueueCharge {
    bytes: usize,
    total: Arc<AtomicUsize>,
}

impl Drop for QueueCharge {
    fn drop(&mut self) {
        self.total.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct Writer {
    sender: mpsc::SyncSender<WriteJob>,
    bytes: Arc<AtomicUsize>,
}

/// A conversation write, applied in send order by one writer thread.
///
/// Keeping a single global queue gives every conversation a total order: an
/// append is visible to the next append for the same file, `flush` is a real
/// barrier, and `clear_all` cannot be overtaken by appends that were queued
/// before it.
enum WriteJob {
    Append {
        path: PathBuf,
        max_bytes: u64,
        /// Already capped before ownership crosses the UI boundary.
        entry: Entry,
        charge: QueueCharge,
        error: Arc<Mutex<Option<String>>>,
    },
    Flush {
        ack: mpsc::Sender<()>,
    },
    Clear {
        ack: mpsc::Sender<io::Result<()>>,
    },
    DeletePane {
        dir: PathBuf,
        ack: mpsc::Sender<io::Result<()>>,
    },
    /// Release one file handle when its `Log` is dropped. The file is reopened
    /// lazily if another `Log` for the same conversation appends again.
    Close {
        path: PathBuf,
    },
}

static WRITER: LazyLock<Writer> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::sync_channel::<WriteJob>(MAX_QUEUED_ENTRIES);
    let _ = std::thread::Builder::new()
        .name("shob-conversation-writer".into())
        .spawn(move || writer_loop(receiver));
    Writer {
        sender,
        bytes: Arc::new(AtomicUsize::new(0)),
    }
});

fn writer() -> &'static Writer {
    &WRITER
}

struct OpenFile {
    file: Option<File>,
    written: u64,
}

fn writer_loop(receiver: mpsc::Receiver<WriteJob>) {
    let mut files: HashMap<PathBuf, OpenFile> = HashMap::new();
    while let Ok(job) = receiver.recv() {
        match job {
            WriteJob::Append {
                path,
                max_bytes,
                entry,
                charge: _charge,
                error: error_sink,
            } => {
                let write_start = std::time::Instant::now();
                let mut line = match serde_json::to_vec(&entry) {
                    Ok(line) => line,
                    Err(error) => {
                        *error_sink.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("Failed to encode conversation history: {error}"));
                        crate::agent_bridge::signal_wake();
                        continue;
                    }
                };
                line.push(b'\n');
                let result = write_line(&mut files, &path, max_bytes, &line);
                crate::stream_timing::record_since("transcript.write", write_start);
                if let Err(error) = result {
                    *error_sink.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("Failed to append conversation history: {error}"));
                    crate::agent_bridge::signal_wake();
                }
            }
            WriteJob::Flush { ack } => {
                for open in files.values_mut() {
                    if let Some(file) = open.file.as_mut() {
                        let _ = file.flush();
                    }
                }
                let _ = ack.send(());
            }
            WriteJob::Clear { ack } => {
                // Drop open handles first so the next append recreates the
                // directory instead of writing through a stale descriptor.
                files.clear();
                let result = (|| {
                    let dir = paths::conversations_dir();
                    if dir.exists() {
                        fs::remove_dir_all(&dir)?;
                    }
                    fs::create_dir_all(dir)
                })();
                let _ = ack.send(result);
            }
            WriteJob::DeletePane { dir, ack } => {
                files.retain(|path, _| !path.starts_with(&dir));
                let result = match fs::remove_dir_all(&dir) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    result => result,
                };
                let _ = ack.send(result);
            }
            WriteJob::Close { path } => {
                files.remove(&path);
            }
        }
    }
}

fn write_line(
    files: &mut HashMap<PathBuf, OpenFile>,
    path: &Path,
    max_bytes: u64,
    line: &[u8],
) -> io::Result<()> {
    if !files.contains_key(path)
        && files.len() >= MAX_OPEN_FILES
        && let Some(oldest) = files.keys().next().cloned()
    {
        files.remove(&oldest);
    }
    let open = files.entry(path.to_path_buf()).or_insert_with(|| OpenFile {
        file: None,
        written: fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
    });
    if open.written > 0 && open.written + line.len() as u64 > max_bytes {
        open.file = None;
        let rotated = path.with_extension("jsonl.1");
        match paths::atomic_replace(path, &rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        open.written = 0;
    }
    if open.file.is_none() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        open.file = Some(OpenOptions::new().create(true).append(true).open(path)?);
    }
    if let Some(file) = open.file.as_mut() {
        file.write_all(line)?;
    }
    open.written += line.len() as u64;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Entry {
    User { text: String },
    Event { event: AgentEvent },
    Local { role: ChatRole, text: String },
}

/// One append-only conversation file.
///
/// Capped prefixes cross a bounded nonblocking queue; `take_error` exposes
/// overload and filesystem failures. Entry format and rotation
/// policy are unchanged, so replay reads exactly what earlier versions wrote.
pub struct Log {
    path: PathBuf,
    max_bytes: u64,
    writer: Writer,
    error: Arc<Mutex<Option<String>>>,
}

impl Log {
    pub fn open(uid: &str, key: &str) -> Self {
        let path = paths::conversations_dir()
            .join(uid)
            .join(format!("{key}.jsonl"));
        Self::at_path(path, MAX_FILE_BYTES)
    }

    pub fn at_path(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            writer: writer().clone(),
            error: Arc::new(Mutex::new(None)),
        }
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_user(&mut self, text: &str) {
        self.append(Entry::User {
            text: capped(text, MAX_ENTRY_BYTES),
        });
    }

    pub fn append_local(&mut self, role: ChatRole, text: &str) {
        self.append(Entry::Local {
            role,
            text: capped(text, MAX_ENTRY_BYTES),
        });
    }

    pub fn append_event(&mut self, event: &AgentEvent) {
        self.append(Entry::Event {
            event: project_event(event),
        });
    }

    fn append(&mut self, entry: Entry) {
        let enqueue_start = std::time::Instant::now();
        let bytes = entry_bytes(&entry).saturating_add(self.path.as_os_str().len());
        let previous = self.writer.bytes.fetch_add(bytes, Ordering::Relaxed);
        let charge = QueueCharge {
            bytes,
            total: Arc::clone(&self.writer.bytes),
        };
        if previous.saturating_add(bytes) > MAX_QUEUED_BYTES {
            drop(charge);
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("Conversation history queue is full; this entry was not saved.".into());
        } else if let Err(error) = self.writer.sender.try_send(WriteJob::Append {
            path: self.path.clone(),
            max_bytes: self.max_bytes,
            entry,
            charge,
            error: Arc::clone(&self.error),
        }) {
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(match error {
                mpsc::TrySendError::Full(_) => {
                    "Conversation history queue is full; this entry was not saved.".into()
                }
                mpsc::TrySendError::Disconnected(_) => {
                    "Conversation history writer stopped; this entry was not saved.".into()
                }
            });
        }
        crate::stream_timing::record_since("transcript.enqueue", enqueue_start);
    }

    /// UI callers must surface this diagnostic without recursively logging it.
    pub fn take_error(&mut self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Block until every write queued before this call has reached the file.
    /// Used by shutdown and by tests that immediately replay the transcript.
    pub fn flush(&mut self) {
        let (ack, receiver) = mpsc::channel();
        if self.writer.sender.send(WriteJob::Flush { ack }).is_ok() {
            let _ = receiver.recv();
        }
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        // Never wait for disk backpressure during pane/session teardown.
        // If full, the bounded open-file cache eventually releases the handle.
        let _ = self.writer.sender.try_send(WriteJob::Close {
            path: self.path.clone(),
        });
    }
}

fn capped(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let suffix = "… (truncated)";
    let mut out = String::with_capacity(end + suffix.len());
    out.push_str(&text[..end]);
    out.push_str(suffix);
    out
}

/// Clone only the persisted prefix, not a multi-megabyte cumulative snapshot.
fn project_event(event: &AgentEvent) -> AgentEvent {
    match event {
        AgentEvent::Text(text) => AgentEvent::Text(capped(text, MAX_ENTRY_BYTES)),
        AgentEvent::Thinking(text) => AgentEvent::Thinking(capped(text, MAX_ENTRY_BYTES)),
        AgentEvent::Status(text) => AgentEvent::Status(capped(text, MAX_ENTRY_BYTES)),
        AgentEvent::Tool(text) => AgentEvent::Tool(capped(text, MAX_ENTRY_BYTES)),
        AgentEvent::ToolStart { id, name, input } => AgentEvent::ToolStart {
            id: id.clone(),
            name: name.clone(),
            input: capped(input, MAX_ENTRY_BYTES),
        },
        AgentEvent::ToolUpdate { id, name, output } => AgentEvent::ToolUpdate {
            id: id.clone(),
            name: name.clone(),
            output: capped(output, MAX_ENTRY_BYTES),
        },
        AgentEvent::ToolEnd {
            id,
            name,
            output,
            failed,
        } => AgentEvent::ToolEnd {
            id: id.clone(),
            name: name.clone(),
            output: capped(output, MAX_ENTRY_BYTES),
            failed: *failed,
        },
        AgentEvent::Subagents { tool_id, workers } => AgentEvent::Subagents {
            tool_id: tool_id.clone(),
            workers: workers
                .iter()
                .map(|worker| crate::subagent_chat::Worker {
                    id: worker.id.clone(),
                    title: worker.title.clone(),
                    status: worker.status.clone(),
                    transcript: worker.transcript.clone(),
                    result: worker
                        .result
                        .as_deref()
                        .map(|text| capped(text, MAX_ENTRY_BYTES)),
                })
                .collect(),
        },
        AgentEvent::Error(text) => AgentEvent::Error(capped(text, MAX_ENTRY_BYTES)),
        AgentEvent::Title(text) => AgentEvent::Title(capped(text, 200)),
        AgentEvent::Usage(usage) => AgentEvent::Usage(*usage),
        AgentEvent::Done => AgentEvent::Done,
        AgentEvent::Closed => AgentEvent::Closed,
    }
}

fn entry_bytes(entry: &Entry) -> usize {
    use embedded::QueuedEvent;
    std::mem::size_of::<Entry>()
        + match entry {
            Entry::User { text } | Entry::Local { text, .. } => text.capacity(),
            Entry::Event { event } => event.queued_bytes(),
        }
}

/// File name stem for a chat: `default` for the model-less conversation, or a
/// sanitized backend/provider/model key.
pub fn conversation_key(model: Option<&AgentModel>) -> String {
    match model {
        None => "default".to_string(),
        Some(model) => {
            let backend = match model.backend {
                ModelBackend::Embedded => "embedded",
                ModelBackend::Shob => "shob",
            };
            sanitize(&format!("{backend}_{}_{}", model.provider, model.id))
        }
    }
}

/// Historical model-specific transcripts used the former backend name.
pub fn legacy_conversation_key(model: Option<&AgentModel>) -> String {
    match model {
        Some(model) if model.backend == ModelBackend::Shob => {
            sanitize(&format!("omp_{}_{}", model.provider, model.id))
        }
        _ => conversation_key(model),
    }
}

fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("default");
    }
    out
}

#[derive(Default)]
pub(crate) struct RestoredConversation {
    pub messages: Vec<Arc<ChatMessage>>,
    pub checklist: Option<embedded::task_list::TaskList>,
}

#[cfg(test)]
fn load(path: &Path) -> Vec<Arc<ChatMessage>> {
    restore(path).messages
}

/// Replay the transcript and the latest checklist for the current turn.
pub(crate) fn restore(path: &Path) -> RestoredConversation {
    let mut restored = RestoredConversation::default();
    let Some(text) = read_bounded(path) else {
        return restored;
    };
    let messages = &mut restored.messages;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(line) else {
            continue;
        };
        match entry {
            Entry::User { text } => {
                restored.checklist = None;
                messages.push(Arc::new(ChatMessage::new(ChatRole::User, text)));
            }
            Entry::Local { role, text } => {
                messages.push(std::sync::Arc::new(ChatMessage::new(role, text)))
            }
            Entry::Event { event } => match event {
                AgentEvent::Done | AgentEvent::Closed => {}
                event => {
                    if let Some(list) = task_list_update(&event) {
                        restored.checklist = Some(list);
                    } else {
                        push_chat_event(messages, event);
                    }
                }
            },
        }
    }
    normalize_replay(messages);
    restored
}

/// Restore cumulative usage from the same bounded history as the transcript.
/// Missing usage or files yield `None`; any unknown message cost stays unknown.
pub fn load_usage(path: &Path) -> Option<embedded::AgentUsage> {
    let text = read_bounded(path)?;
    let mut total: Option<embedded::AgentUsage> = None;
    for line in text.lines() {
        let Ok(Entry::Event {
            event: AgentEvent::Usage(usage),
        }) = serde_json::from_str::<Entry>(line.trim())
        else {
            continue;
        };
        if let Some(total) = total.as_mut() {
            total.input = total.input.saturating_add(usage.input);
            total.output = total.output.saturating_add(usage.output);
            total.cache_read = total.cache_read.saturating_add(usage.cache_read);
            total.cache_write = total.cache_write.saturating_add(usage.cache_write);
            total.cost = total.cost.zip(usage.cost).map(|(total, cost)| total + cost);
        } else {
            total = Some(usage);
        }
    }
    total
}

fn read_bounded(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > MAX_FILE_BYTES {
        file.seek(SeekFrom::Start(len - MAX_FILE_BYTES)).ok()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // Drop the partial first line after seeking into the middle of a file.
        let start = text.find('\n').map(|index| index + 1).unwrap_or(text.len());
        Some(text[start..].to_string())
    } else {
        let mut text = String::new();
        file.read_to_string(&mut text).ok()?;
        Some(text)
    }
}

/// State that was mid-flight when the app quit must not look live after a
/// restart: no streaming message and no spinning tool calls.
fn normalize_replay(messages: &mut [std::sync::Arc<ChatMessage>]) {
    for message in messages {
        let message = std::sync::Arc::make_mut(message);
        message.streaming = false;
        if let Some(tool) = message.tool.as_mut()
            && tool.state == ToolState::Running
        {
            tool.state = ToolState::Unknown;
        }
    }
}

/// Delete every conversation, ordered behind all writes already queued. New
/// appends sent after this barrier are new history and are allowed to recreate
/// the directory.
pub fn clear_all() -> io::Result<()> {
    let (ack, receiver) = mpsc::channel();
    writer()
        .sender
        .send(WriteJob::Clear { ack })
        .map_err(|_| io::Error::other("conversation writer stopped"))?;
    receiver
        .recv()
        .map_err(|_| io::Error::other("conversation writer did not acknowledge clear"))?
}

/// Delete a pane's conversations after all previously queued writes.
pub fn delete_pane(uid: &str) -> io::Result<()> {
    delete_directory(paths::conversations_dir().join(uid))
}

fn delete_directory(dir: PathBuf) -> io::Result<()> {
    let (ack, receiver) = mpsc::channel();
    writer()
        .sender
        .send(WriteJob::DeletePane { dir, ack })
        .map_err(|_| io::Error::other("conversation writer stopped"))?;
    receiver
        .recv()
        .map_err(|_| io::Error::other("conversation writer did not acknowledge deletion"))?
}

#[cfg(test)]
#[path = "conversations/tests.rs"]
mod tests;
