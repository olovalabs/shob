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
        .name("kathamo-conversation-writer".into())
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
    if !files.contains_key(path) && files.len() >= MAX_OPEN_FILES {
        if let Some(oldest) = files.keys().next().cloned() {
            files.remove(&oldest);
        }
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
    use rpi_embedded::QueuedEvent;
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
                ModelBackend::Rpi => "rpi",
                ModelBackend::Kathamo => "kathamo",
            };
            sanitize(&format!("{backend}_{}_{}", model.provider, model.id))
        }
    }
}

/// Historical model-specific transcripts used the former backend name.
pub fn legacy_conversation_key(model: Option<&AgentModel>) -> String {
    match model {
        Some(model) if model.backend == ModelBackend::Kathamo => {
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
    pub checklist: Option<rpi_embedded::task_list::TaskList>,
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
pub fn load_usage(path: &Path) -> Option<rpi_embedded::AgentUsage> {
    let text = read_bounded(path)?;
    let mut total: Option<rpi_embedded::AgentUsage> = None;
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
mod tests {
    use super::*;

    #[test]
    fn delete_pane_waits_for_queued_writes_and_preserves_other_panes() {
        let root = std::env::temp_dir().join(crate::paths::uid("conversation-delete-test"));
        let deleted = root.join("deleted");
        let retained = root.join("retained");
        let mut first = Log::at_path(deleted.join("one.jsonl"), u64::MAX);
        let mut second = Log::at_path(retained.join("two.jsonl"), u64::MAX);
        first.append_user("private");
        second.append_user("keep");
        delete_directory(deleted.clone()).unwrap();
        assert!(!deleted.exists());
        assert!(retained.join("two.jsonl").exists());
        delete_directory(deleted).unwrap();
        drop(first);
        drop(second);
        let _ = fs::remove_dir_all(root);
    }

    fn temp_log(name: &str, max_bytes: u64) -> Log {
        let path = std::env::temp_dir().join(format!(
            "kathamo-conversations-{name}-{}.jsonl",
            paths::uid("t")
        ));
        Log::at_path(path, max_bytes)
    }

    #[test]
    fn entries_roundtrip_as_json() {
        let entries = vec![
            Entry::User {
                text: "hello".into(),
            },
            Entry::Event {
                event: AgentEvent::Text("hi".into()),
            },
            Entry::Event {
                event: AgentEvent::ToolStart {
                    id: Some("1".into()),
                    name: "read".into(),
                    input: "{}".into(),
                },
            },
            Entry::Local {
                role: ChatRole::Status,
                text: "/fast on".into(),
            },
        ];
        for entry in entries {
            let text = serde_json::to_string(&entry).unwrap();
            let _: Entry = serde_json::from_str(&text).unwrap();
        }
    }

    #[test]
    fn transcript_replays_user_and_agent_messages() {
        let mut log = temp_log("replay", MAX_FILE_BYTES);
        log.append_user("fix the bug");
        log.append_event(&AgentEvent::Text("looking".into()));
        log.append_event(&AgentEvent::Text(" now".into()));
        log.append_event(&AgentEvent::ToolStart {
            id: Some("t1".into()),
            name: "read".into(),
            input: "src/main.rs".into(),
        });
        log.append_event(&AgentEvent::ToolEnd {
            id: Some("t1".into()),
            name: "read".into(),
            output: "ok".into(),
            failed: false,
        });
        log.append_local(ChatRole::Status, "/fast on");
        log.flush();
        let messages = load(log.path());

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, ChatRole::User);
        assert_eq!(messages[0].text, "fix the bug");
        // Consecutive text deltas coalesce into one assistant message.
        assert_eq!(messages[1].role, ChatRole::Assistant);
        assert_eq!(messages[1].text, "looking now");
        assert!(!messages[1].streaming);
        assert_eq!(messages[2].role, ChatRole::Tool);
        let tool = messages[2].tool.as_ref().unwrap();
        assert_eq!(tool.state, ToolState::Completed);
        assert_eq!(tool.output, "ok");
        assert_eq!(messages[3].role, ChatRole::Status);
        assert_eq!(messages[3].text, "/fast on");

        let path = log.path().to_path_buf();
        drop(log);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn usage_replays_multiple_finalized_messages() {
        let mut log = temp_log("usage-messages", MAX_FILE_BYTES);
        log.append_user("first");
        log.append_event(&AgentEvent::Text("answer".into()));
        log.append_event(&AgentEvent::Usage(rpi_embedded::AgentUsage {
            input: 100,
            output: 20,
            cache_read: 40,
            cache_write: 10,
            cost: Some(0.125),
        }));
        log.append_event(&AgentEvent::Done);
        log.append_user("second");
        log.append_event(&AgentEvent::Text("another answer".into()));
        log.append_event(&AgentEvent::Usage(rpi_embedded::AgentUsage {
            input: 200,
            output: 30,
            cache_read: 80,
            cache_write: 15,
            cost: Some(0.25),
        }));
        log.append_event(&AgentEvent::Done);
        log.flush();

        assert_eq!(
            load_usage(log.path()),
            Some(rpi_embedded::AgentUsage {
                input: 300,
                output: 50,
                cache_read: 120,
                cache_write: 25,
                cost: Some(0.375),
            })
        );
        let messages = load(log.path());
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].text, "answer");
        assert_eq!(messages[3].text, "another answer");
        assert!(messages.iter().all(|message| !message.streaming));
        // Replaying is read-only and never adds the deltas a second time.
        assert_eq!(
            load_usage(log.path()),
            Some(rpi_embedded::AgentUsage {
                input: 300,
                output: 50,
                cache_read: 120,
                cache_write: 25,
                cost: Some(0.375),
            })
        );

        let path = log.path().to_path_buf();
        drop(log);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn usage_replay_preserves_unknown_cost() {
        for costs in [[Some(0.25), None, Some(0.5)], [None, Some(0.25), Some(0.5)]] {
            let mut log = temp_log("usage-unknown-cost", MAX_FILE_BYTES);
            for cost in costs {
                log.append_event(&AgentEvent::Usage(rpi_embedded::AgentUsage {
                    input: 10,
                    output: 2,
                    cache_read: 3,
                    cache_write: 4,
                    cost,
                }));
            }
            log.flush();
            assert_eq!(
                load_usage(log.path()),
                Some(rpi_embedded::AgentUsage {
                    input: 30,
                    output: 6,
                    cache_read: 9,
                    cache_write: 12,
                    cost: None,
                })
            );
            let path = log.path().to_path_buf();
            drop(log);
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn usage_replay_skips_corrupt_lines_and_absent_usage() {
        let mut log = temp_log("usage-corrupt", MAX_FILE_BYTES);
        assert_eq!(load_usage(log.path()), None);
        log.append_user("hello");
        log.flush();
        assert_eq!(load_usage(log.path()), None);

        let usage = rpi_embedded::AgentUsage {
            input: 15,
            output: 5,
            cache_read: 2,
            cache_write: 1,
            cost: Some(0.0),
        };
        let entry = serde_json::to_string(&Entry::Event {
            event: AgentEvent::Usage(usage),
        })
        .unwrap();
        fs::write(
            log.path(),
            format!("not json\n\n{entry}\n{{\"t\":\"event\",\"event\":null}}\n{entry}\n{{"),
        )
        .unwrap();
        assert_eq!(
            load_usage(log.path()),
            Some(rpi_embedded::AgentUsage {
                input: 30,
                output: 10,
                cache_read: 4,
                cache_write: 2,
                cost: Some(0.0),
            })
        );
        assert!(load(log.path()).is_empty());

        let path = log.path().to_path_buf();
        drop(log);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn usage_replay_uses_the_transcript_read_bound() {
        let log = temp_log("usage-bounded", MAX_FILE_BYTES);
        let usage = rpi_embedded::AgentUsage {
            input: 10,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            cost: Some(0.125),
        };
        let entry = serde_json::to_string(&Entry::Event {
            event: AgentEvent::Usage(usage),
        })
        .unwrap();
        let mut file = File::create(log.path()).unwrap();
        writeln!(file, "{entry}").unwrap();
        // A sparse oversized line puts the first usage outside the read window.
        file.seek(SeekFrom::Start(MAX_FILE_BYTES + 1024)).unwrap();
        writeln!(file).unwrap();
        writeln!(file, "{entry}").unwrap();
        writeln!(
            file,
            "{}",
            serde_json::to_string(&Entry::User {
                text: "retained".into(),
            })
            .unwrap()
        )
        .unwrap();
        drop(file);

        assert_eq!(load_usage(log.path()), Some(usage));
        let messages = load(log.path());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "retained");
        let path = log.path().to_path_buf();
        drop(log);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn queued_writes_keep_per_conversation_order() {
        let mut first = temp_log("order-first", MAX_FILE_BYTES);
        let mut second = temp_log("order-second", MAX_FILE_BYTES);
        first.append_event(&AgentEvent::Text("one".into()));
        second.append_event(&AgentEvent::Text("other".into()));
        first.append_event(&AgentEvent::Text(" two".into()));
        first.flush();

        let first_messages = load(first.path());
        assert_eq!(first_messages.len(), 1);
        assert_eq!(first_messages[0].text, "one two");
        let second_messages = load(second.path());
        assert_eq!(second_messages.len(), 1);
        assert_eq!(second_messages[0].text, "other");

        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();
        drop(first);
        drop(second);
        let _ = fs::remove_file(first_path);
        let _ = fs::remove_file(second_path);
    }

    #[test]
    fn running_tools_are_not_left_spinning() {
        let mut log = temp_log("running", MAX_FILE_BYTES);
        log.append_event(&AgentEvent::ToolStart {
            id: Some("t1".into()),
            name: "read".into(),
            input: "".into(),
        });
        log.flush();
        let messages = load(log.path());
        assert_eq!(messages[0].tool.as_ref().unwrap().state, ToolState::Unknown);
        let path = log.path().to_path_buf();
        drop(log);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn large_logs_rotate_and_keep_complete_lines() {
        let mut log = temp_log("rotate", 300);
        for index in 0..20 {
            log.append_event(&AgentEvent::Text(format!("line {index}")));
        }
        log.flush();
        let rotated = log.path().with_extension("jsonl.1");
        assert!(rotated.exists(), "expected the first generation to rotate");
        let current = load(log.path());
        assert!(!current.is_empty());
        assert!(fs::metadata(log.path()).unwrap().len() <= 300);
        let path = log.path().to_path_buf();
        drop(log);
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(rotated);
    }

    #[test]
    fn conversation_keys_are_filesystem_safe() {
        let model = AgentModel {
            provider: "google/antigravity".into(),
            id: "gemini:3.1".into(),
            backend: ModelBackend::Kathamo,
        };
        let key = conversation_key(Some(&model));
        assert_eq!(key, "kathamo_google_antigravity_gemini_3.1");
        assert_eq!(conversation_key(None), "default");
    }

    #[test]
    fn capped_text_respects_utf8_boundaries() {
        let text = "α".repeat(100);
        let capped = capped(&text, 10);
        assert!(capped.starts_with("ααααα"));
        assert!(capped.ends_with("(truncated)"));
    }

    #[test]
    fn projection_caps_tool_results_without_changing_live_payload() {
        let event = AgentEvent::ToolEnd {
            id: Some("call".into()),
            name: "bash".into(),
            output: "α".repeat(2 * 1024 * 1024),
            failed: true,
        };
        let projected = project_event(&event);
        assert!(
            matches!(&event, AgentEvent::ToolEnd { output, .. } if output.len() == 4 * 1024 * 1024)
        );
        assert!(
            matches!(&projected, AgentEvent::ToolEnd { id, name, output, failed }
            if id.as_deref() == Some("call") && name == "bash" && *failed &&
                output.len() < MAX_ENTRY_BYTES + 32 && output.ends_with("(truncated)"))
        );
        assert!(matches!(project_event(&AgentEvent::Done), AgentEvent::Done));
        assert!(
            matches!(project_event(&AgentEvent::Error("error".into())), AgentEvent::Error(text) if text == "error")
        );
    }

    #[test]
    fn stalled_history_writer_reports_backpressure_without_waiting() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let bytes = Arc::new(AtomicUsize::new(0));
        let mut log = Log {
            path: PathBuf::from("stalled-history"),
            max_bytes: MAX_FILE_BYTES,
            writer: Writer {
                sender,
                bytes: Arc::clone(&bytes),
            },
            error: Arc::new(Mutex::new(None)),
        };
        log.append_event(&AgentEvent::Text("accepted".into()));
        let charged = bytes.load(Ordering::Relaxed);
        assert!(charged > 0);
        let start = std::time::Instant::now();
        log.append_event(&AgentEvent::Done);
        assert!(start.elapsed() < std::time::Duration::from_millis(50));
        assert!(log.take_error().unwrap().contains("queue is full"));
        assert!(log.take_error().is_none());
        assert_eq!(bytes.load(Ordering::Relaxed), charged);
        drop(receiver.recv().unwrap());
        assert_eq!(bytes.load(Ordering::Relaxed), 0);
        drop(receiver);
        log.append_user("disconnected");
        assert!(log.take_error().unwrap().contains("writer stopped"));
        assert_eq!(bytes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn history_byte_budget_and_disk_errors_are_observable() {
        let (sender, receiver) = mpsc::sync_channel(MAX_QUEUED_ENTRIES);
        let bytes = Arc::new(AtomicUsize::new(0));
        let mut log = Log {
            path: PathBuf::from("byte-budget"),
            max_bytes: MAX_FILE_BYTES,
            writer: Writer {
                sender,
                bytes: Arc::clone(&bytes),
            },
            error: Arc::new(Mutex::new(None)),
        };
        let event = AgentEvent::Text("x".repeat(MAX_ENTRY_BYTES));
        for _ in 0..MAX_QUEUED_ENTRIES {
            log.append_event(&event);
        }
        assert!(bytes.load(Ordering::Relaxed) <= MAX_QUEUED_BYTES);
        assert!(log.take_error().unwrap().contains("queue is full"));
        drop(receiver);
        assert_eq!(bytes.load(Ordering::Relaxed), 0);

        let file = std::env::temp_dir().join(paths::uid("history-error"));
        fs::write(&file, "not a directory").unwrap();
        let mut failed = Log::at_path(file.join("history.jsonl"), MAX_FILE_BYTES);
        failed.append_user("hello");
        failed.flush();
        assert!(failed.take_error().unwrap().contains("Failed to append"));
        drop(failed);
        fs::remove_file(file).unwrap();
    }

    #[test]
    #[ignore = "manual transcript projection cost comparison"]
    fn transcript_projection_benchmark() {
        let event = AgentEvent::ToolUpdate {
            id: Some("bench".into()),
            name: "bash".into(),
            output: "x".repeat(1024 * 1024),
        };
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(project_event(std::hint::black_box(&event)));
        }
        let capped = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(std::hint::black_box(&event).clone());
        }
        eprintln!(
            "transcript projection: capped={capped:?}, full_clone={:?}, retained_prefix_limit={MAX_ENTRY_BYTES}",
            start.elapsed()
        );
    }
}
