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
use crate::{ChatMessage, ChatRole, ToolState, task_list_update};
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
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_ms: Option<u64>,
    },
    Event {
        event: AgentEvent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_ms: Option<u64>,
    },
    Local {
        role: ChatRole,
        text: String,
    },
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

    #[cfg(test)]
    pub fn append_user(&mut self, text: &str) {
        self.append_user_at(text, crate::chat_timestamp_ms());
    }

    pub fn append_user_at(&mut self, text: &str, at_ms: Option<u64>) {
        self.append(Entry::User {
            text: capped(text, MAX_ENTRY_BYTES),
            at_ms,
        });
    }

    pub fn append_local(&mut self, role: ChatRole, text: &str) {
        self.append(Entry::Local {
            role,
            text: capped(text, MAX_ENTRY_BYTES),
        });
    }

    #[cfg(test)]
    pub fn append_event(&mut self, event: &AgentEvent) {
        self.append_event_at(event, crate::chat_timestamp_ms());
    }

    pub fn append_event_at(&mut self, event: &AgentEvent, at_ms: Option<u64>) {
        // Native worker previews are replaceable snapshots. The child transcript
        // and final tool result are durable, not repeated copies of the live tail.
        if matches!(event, AgentEvent::ToolUpdate { name, .. } if name == embedded::subagent_tool::NAME)
        {
            return;
        }
        self.append(Entry::Event {
            event: project_event(event),
            at_ms,
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
            Entry::User { text, .. } | Entry::Local { text, .. } => text.capacity(),
            Entry::Event { event, .. } => event.queued_bytes(),
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
            Entry::User { text, at_ms } => {
                restored.checklist = None;
                let mut message = ChatMessage::new(ChatRole::User, text);
                message.source_timestamp = at_ms;
                messages.push(Arc::new(message));
            }
            Entry::Local { role, text } => {
                messages.push(std::sync::Arc::new(ChatMessage::new(role, text)))
            }
            Entry::Event { event, at_ms } => match event {
                AgentEvent::Done | AgentEvent::Closed => {
                    crate::finish_worked_turn(messages, at_ms);
                }
                event => {
                    if let Some(list) = task_list_update(&event) {
                        restored.checklist = Some(list);
                    } else {
                        crate::push_chat_event_at(messages, event, at_ms);
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
            ..
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

/// Search the persisted user and assistant text without building chat rows,
/// compiling Markdown or replaying tool output.
pub(crate) struct MessageSearch {
    query: String,
    folded: String,
}

struct MessageHit {
    role: ChatRole,
    text: String,
    at_ms: Option<u64>,
}

impl MessageSearch {
    pub(crate) fn new(query: &str) -> Self {
        Self {
            query: folded_message_chars(query).map(|(_, ch)| ch).collect(),
            folded: String::new(),
        }
    }

    /// Return the newest matching excerpt across a thread's pane logs.
    /// Missing logs are normal; keep scanning other panes after an I/O error.
    pub(crate) fn search_panes(
        &mut self,
        root: &Path,
        pane_uids: &[String],
        is_stale: impl Fn() -> bool,
    ) -> (Option<(ChatRole, String)>, bool) {
        let mut newest = None;
        let mut failed = false;
        for uid in pane_uids {
            if is_stale() {
                break;
            }
            let dir = root.join(uid);
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) => {
                    failed |= error.kind() != io::ErrorKind::NotFound;
                    continue;
                }
            };
            for entry in entries {
                if is_stale() {
                    break;
                }
                let result = entry.and_then(|entry| {
                    let name = entry.file_name();
                    if Path::new(&name)
                        .extension()
                        .is_none_or(|ext| ext != "jsonl")
                        || !entry.file_type()?.is_file()
                    {
                        return Ok(None);
                    }
                    let mut file = File::open(dir.join(name))?;
                    let meta = file.metadata()?;
                    let snapshot = read_snapshot(&mut file, meta.len(), MAX_FILE_BYTES)?;
                    let mut hit = self.search_transcript(&snapshot, &is_stale);
                    if let Some(hit) = hit.as_mut()
                        && hit.at_ms.is_none()
                    {
                        hit.at_ms = meta
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
                    }
                    Ok(hit)
                });
                match result {
                    Ok(Some(hit))
                        if newest
                            .as_ref()
                            .is_none_or(|old: &MessageHit| old.at_ms < hit.at_ms) =>
                    {
                        newest = Some(hit);
                    }
                    Err(error) => failed |= error.kind() != io::ErrorKind::NotFound,
                    _ => {}
                }
            }
        }
        (newest.map(|hit| (hit.role, hit.text)), failed)
    }

    fn search_transcript(
        &mut self,
        snapshot: &str,
        is_stale: &impl Fn() -> bool,
    ) -> Option<MessageHit> {
        let mut newest = None;
        let mut assistant = String::new();
        let mut assistant_at = None;
        for line in snapshot.lines() {
            if is_stale() {
                return None;
            }
            let Ok(entry) = serde_json::from_str::<Entry>(line) else {
                continue; // As in replay, skip a corrupt or partially written line.
            };
            let (role, text, at_ms) = match entry {
                Entry::Event {
                    event: AgentEvent::Text(delta),
                    at_ms,
                } => {
                    assistant.push_str(&delta);
                    assistant_at = at_ms.or(assistant_at);
                    continue;
                }
                Entry::Event {
                    event:
                        AgentEvent::Title(_) | AgentEvent::Usage(_) | AgentEvent::Subagents { .. },
                    ..
                } => continue,
                Entry::User { text, at_ms } => (ChatRole::User, text, at_ms),
                Entry::Local { role, text } => (role, text, None),
                Entry::Event { .. } => {
                    self.record_hit(
                        &mut newest,
                        ChatRole::Assistant,
                        &assistant,
                        assistant_at.take(),
                    );
                    assistant.clear();
                    continue;
                }
            };
            self.record_hit(
                &mut newest,
                ChatRole::Assistant,
                &assistant,
                assistant_at.take(),
            );
            assistant.clear();
            self.record_hit(&mut newest, role, &text, at_ms);
        }
        self.record_hit(&mut newest, ChatRole::Assistant, &assistant, assistant_at);
        newest
    }

    fn record_hit(
        &mut self,
        hit: &mut Option<MessageHit>,
        role: ChatRole,
        text: &str,
        at_ms: Option<u64>,
    ) {
        if self.query.is_empty()
            || text.is_empty()
            || !matches!(role, ChatRole::User | ChatRole::Assistant)
        {
            return;
        }
        self.folded.clear();
        self.folded
            .extend(folded_message_chars(text).map(|(_, ch)| ch));
        let Some(offset) = self.folded.find(&self.query) else {
            return;
        };
        // Keep offsets in the original text even when folding changes a
        // character's UTF-8 width or turns several whitespace characters into one.
        let mut folded_offset = 0;
        let Some(found) = folded_message_chars(text).find_map(|(index, ch)| {
            let here = folded_offset == offset;
            folded_offset += ch.len_utf8();
            here.then_some(index)
        }) else {
            return;
        };
        let start = text[..found]
            .char_indices()
            .rev()
            .nth(16)
            .map_or(0, |(index, _)| index);
        let end = text[start..]
            .char_indices()
            .nth(128)
            .map_or(text.len(), |(index, _)| start + index);
        let mut excerpt = String::new();
        if start != 0 {
            excerpt.push_str("… ");
        }
        for (index, word) in text[start..end].split_whitespace().enumerate() {
            if index != 0 {
                excerpt.push(' ');
            }
            excerpt.push_str(word);
        }
        if end != text.len() {
            excerpt.push('…');
        }
        *hit = Some(MessageHit {
            role,
            text: excerpt,
            at_ms,
        });
    }
}

fn folded_message_chars(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut in_whitespace = true;
    text.trim_end()
        .char_indices()
        .filter_map(move |(index, ch)| {
            let whitespace = ch.is_whitespace();
            let was_whitespace = std::mem::replace(&mut in_whitespace, whitespace);
            match ch {
                _ if whitespace => (!was_whitespace).then_some((index, ' ')),
                '‘' | '’' => Some((index, '\'')),
                'ς' => Some((index, 'σ')),
                _ => Some((index, ch)),
            }
        })
        .flat_map(|(index, ch)| ch.to_lowercase().map(move |folded| (index, folded)))
}

#[cfg(test)]
#[test]
fn sidebar_message_search_joins_assistant_deltas_and_ignores_non_messages() {
    let entries = [
        Entry::User {
            text: "A different question".into(),
            at_ms: Some(1),
        },
        Entry::Event {
            event: AgentEvent::Text("Context before the excerpt: you".into()),
            at_ms: Some(2),
        },
        Entry::Event {
            event: AgentEvent::Title("Internal notes".into()),
            at_ms: None,
        },
        Entry::Event {
            event: AgentEvent::Text("’ve found\n the fix.".into()),
            at_ms: Some(3),
        },
        Entry::Event {
            event: AgentEvent::Thinking("you've found the fix but don't show this".into()),
            at_ms: Some(4),
        },
    ];
    let snapshot = entries
        .iter()
        .map(|entry| serde_json::to_string(entry).unwrap())
        .collect::<Vec<_>>()
        .join("\n");

    let hit = MessageSearch::new("YOU'VE FOUND THE FIX")
        .search_transcript(&snapshot, &|| false)
        .unwrap();
    assert_eq!(hit.role, ChatRole::Assistant);
    assert_eq!(hit.at_ms, Some(3));
    assert!(hit.text.ends_with("you’ve found the fix."));
    assert!(
        MessageSearch::new("Internal notes")
            .search_transcript(&snapshot, &|| false)
            .is_none()
    );
    assert!(
        MessageSearch::new("you've")
            .search_transcript(&snapshot, &|| true)
            .is_none()
    );
}

fn read_bounded(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    read_snapshot(&mut file, len, MAX_FILE_BYTES).ok()
}

fn read_snapshot(file: &mut File, len: u64, max_bytes: u64) -> io::Result<String> {
    if len <= max_bytes {
        let mut text = String::new();
        file.take(len).read_to_string(&mut text)?;
        return Ok(text);
    }

    file.seek(SeekFrom::Start(len - max_bytes - 1))?;
    let mut preceding = [0];
    file.read_exact(&mut preceding)?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    let start = if preceding[0] == b'\n' {
        0
    } else {
        bytes
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(bytes.len(), |index| index + 1)
    };
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
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
            if tool.name == embedded::subagent_tool::NAME {
                tool.workers
                    .iter_mut()
                    .for_each(crate::subagent_chat::Worker::mark_interrupted);
            }
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
