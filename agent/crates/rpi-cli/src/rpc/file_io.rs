//! Optional native RPC text-I/O transport used by the production dispatcher.
//!
//! The RPC owner must retain `FileIOClient` and call `close` on EOF/shutdown.
//! Backends retain only a Weak reference; even in-flight strong references cannot
//! defer owner-drop cancellation. No harness, adapter or MCP ABI is retained.
use super::{
    client_requests::{ClientRequestBroker, MAX_TIMEOUT_MS},
    writer::RpcWriter,
};
use async_trait::async_trait;
use rpi_tools::file_io::{
    resolve_client_path, FileIOBackend, FileIOError, FileIOErrorCode, FileIOReadRange,
    FileIORequest, FileIOResponse, MAX_FILE_IO_BYTES,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

// Encoded JSON, excluding newline. The native input framing limit (1 MiB)
// remains the first allocation boundary. This stricter budget is checked by a
// streaming counter before cloning a known reply. Highly escaped text can hit
// this budget before the independent 256 KiB UTF-8 content limit.
const MAX_PAYLOAD_BYTES: usize = 768 * 1024;
const MAX_ERROR_BYTES: usize = 4 * 1024;

pub(crate) fn capabilities() -> Value {
    json!({"version": 1, "maxPending": 64, "maxTextBytes": MAX_FILE_IO_BYTES,
        "maxPayloadBytes": MAX_PAYLOAD_BYTES, "timeoutMs": MAX_TIMEOUT_MS})
}

/// Unique lifecycle owner: intentionally NOT Clone (nor Arc-returning).
pub(crate) struct FileIOClient {
    inner: Arc<Inner>,
}
struct Inner {
    broker: ClientRequestBroker,
    gate: Mutex<State>,
    stop: CancellationToken,
    timeout: Duration,
    // Synchronous hook forces the race within one poll, after select has
    // checked cancellation but before the real publication gate is taken.
    #[cfg(test)]
    before_publish: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
struct State {
    closed: bool,
    writer: Option<RpcWriter>,
    // Only unresolved IDs; the broker independently retains admission slots
    // until their waiter drops, including resolved-but-unconsumed replies.
    pending: HashMap<String, Pending>,
}
struct Pending {
    expected: Expected,
    closure: Arc<Closure>,
}
#[derive(Clone, Copy)]
enum Expected {
    Read { max_bytes: usize },
    Write,
}
struct Backend(Weak<Inner>);

impl FileIOClient {
    pub(crate) fn new(writer: RpcWriter) -> Self {
        Self::with_timeout(writer, Duration::from_millis(MAX_TIMEOUT_MS))
    }
    fn with_timeout(writer: RpcWriter, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                broker: ClientRequestBroker::default(),
                gate: Mutex::new(State {
                    closed: false,
                    writer: Some(writer),
                    pending: HashMap::new(),
                }),
                stop: CancellationToken::new(),
                timeout,
                #[cfg(test)]
                before_publish: Mutex::new(None),
            }),
        }
    }
    pub(crate) fn backend(&self) -> Arc<dyn FileIOBackend> {
        Arc::new(Backend(Arc::downgrade(&self.inner)))
    }
    pub(crate) fn close(&self) {
        // Same gate as registration AND publication: close cannot be followed
        // by a request enqueue, including one already holding queue capacity.
        let mut state = self.inner.gate.lock().expect("file I/O gate poisoned");
        state.closed = true;
        self.inner.stop.cancel();
        // Close BEFORE dropping broker senders (which wakes waiters).
        for (_, pending) in state.pending.drain() {
            pending.closure.close();
        }
        self.inner.broker.cancel_all();
        state.writer.take();
    }

    /// Native `file_io_response` envelope after the dispatcher removes only its
    /// optional correlation `id`. Unknown/late/duplicate request IDs
    /// return false BEFORE size scanning or copying any untrusted payload.
    /// Malformed known replies resolve a fixed InvalidResponse and retire.
    pub(crate) fn resolve_response(&self, message: &Value) -> bool {
        let Some(id) = message.get("requestId").and_then(Value::as_str) else {
            return false;
        };
        let mut state = self.inner.gate.lock().expect("file I/O gate poisoned");
        let Some(pending) = state.pending.remove(id) else {
            return false;
        };
        let reply = if valid_reply(message, pending.expected) {
            // Only now allocate a copy, and never preserve arbitrary metadata.
            if let Some(result) = message.get("result") {
                json!({"result": result.clone()})
            } else {
                json!({"error": message["error"].clone()})
            }
        } else {
            json!({"error": {"code": "invalid_response", "message": "invalid client file I/O response"}})
        };
        // Queue the one closure BEFORE waking the consuming broker future.
        pending.closure.close();
        self.inner.broker.resolve(id, Ok(reply))
    }
}
impl Drop for FileIOClient {
    fn drop(&mut self) {
        self.close();
    }
}

fn error(code: FileIOErrorCode) -> FileIOError {
    let message = match code {
        FileIOErrorCode::Unsupported => "file I/O pending request capacity exhausted",
        FileIOErrorCode::InvalidResponse => "invalid client file I/O response",
        FileIOErrorCode::TooLarge => "client file I/O exceeds bounded payload limit",
        FileIOErrorCode::Timeout => {
            "client file I/O timed out; a dispatched write may have committed"
        }
        FileIOErrorCode::Cancelled => {
            "client file I/O cancelled; a dispatched write may have committed"
        }
        FileIOErrorCode::InvalidPath => "unsupported or ambiguous client path",
        _ => "client file I/O disconnected; a dispatched write may have committed",
    };
    FileIOError::new(code, message)
}
fn parse_code(code: &str) -> Option<FileIOErrorCode> {
    Some(match code {
        "unsupported" => FileIOErrorCode::Unsupported,
        "denied" => FileIOErrorCode::Denied,
        "not_found" => FileIOErrorCode::NotFound,
        "invalid_response" => FileIOErrorCode::InvalidResponse,
        "too_large" => FileIOErrorCode::TooLarge,
        "timeout" => FileIOErrorCode::Timeout,
        "cancelled" => FileIOErrorCode::Cancelled,
        "disconnected" => FileIOErrorCode::Disconnected,
        "invalid_path" => FileIOErrorCode::InvalidPath,
        _ => return None,
    })
}

/// Bounded serialization without a second full JSON buffer.
fn fits_payload(value: &Value) -> bool {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_PAYLOAD_BYTES.saturating_sub(self.0) {
                return Err(io::Error::other("file I/O payload limit"));
            }
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), value).is_ok()
}
fn valid_reply(message: &Value, expected: Expected) -> bool {
    let Some(object) = message.as_object() else {
        return false;
    };
    if object.len() != 3
        || message.get("type").and_then(Value::as_str) != Some("file_io_response")
        || !fits_payload(message)
    {
        return false;
    }
    match (message.get("result"), message.get("error")) {
        (Some(result), None) => {
            let Some(result) = result.as_object() else {
                return false;
            };
            match expected {
                Expected::Read { max_bytes } => {
                    result.len() == 1
                        && result
                            .get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.len() <= max_bytes)
                }
                Expected::Write => result.is_empty(),
            }
        }
        (None, Some(remote)) => remote.as_object().is_some_and(|object| {
            object.len() == 2
                && remote
                    .get("code")
                    .and_then(Value::as_str)
                    .and_then(parse_code)
                    .is_some()
                && remote
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.len() <= MAX_ERROR_BYTES)
        }),
        _ => false,
    }
}

fn encode_request(request: FileIORequest) -> Result<(Value, Expected), FileIOError> {
    let (path, mut event, expected) = match request {
        FileIORequest::ReadText {
            absolute_path,
            range,
            max_bytes,
        } => {
            let max_bytes = max_bytes.min(MAX_FILE_IO_BYTES);
            let mut event = json!({"operation": "read_text_file", "maxBytes": max_bytes});
            if let FileIOReadRange::Lines {
                offset_1based,
                limit,
            } = range
            {
                if offset_1based == 0 || limit == 0 {
                    return Err(error(FileIOErrorCode::InvalidResponse));
                }
                event["line"] = json!(offset_1based);
                event["limit"] = json!(limit);
            }
            (absolute_path, event, Expected::Read { max_bytes })
        }
        FileIORequest::WriteText {
            absolute_path,
            content,
        } => {
            if content.len() > MAX_FILE_IO_BYTES {
                return Err(error(FileIOErrorCode::TooLarge));
            }
            (
                absolute_path,
                json!({"operation": "write_text_file", "content": content}),
                Expected::Write,
            )
        }
    };
    // Empty cwd deliberately forbids relative input; use the exact neutral
    // tools resolver for POSIX, drive-absolute, UNC and lexical normalization.
    event["path"] = json!(resolve_client_path(Path::new(""), &path)?);
    event["type"] = json!("file_io_request");
    Ok((event, expected))
}

/// Shared with response/owner-close paths, without retaining Inner (no cycle).
struct Closure {
    writer: RpcWriter,
    id: String,
    published: AtomicBool,
    closed: AtomicBool,
}
impl Closure {
    // All publication and close calls hold Inner::gate. Registration alone is
    // not a client-visible request and must never produce a closed event.
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) && self.published.load(Ordering::Acquire) {
            self.writer.try_write_line(&json!({
                "type": "file_io_request_closed", "requestId": self.id,
            }));
        }
    }
}
/// Declared AFTER the pinned broker waiter. Cancellation/drop closes and removes
/// response admission BEFORE that waiter releases its broker slot. Successful
/// replies close in resolve_response BEFORE the broker sender wakes the waiter.
struct CloseRequest {
    inner: Arc<Inner>,
    closure: Arc<Closure>,
}
impl CloseRequest {
    fn close(&self) {
        let mut state = self.inner.gate.lock().expect("file I/O gate poisoned");
        state.pending.remove(&self.closure.id);
        self.closure.close();
    }
}
impl Drop for CloseRequest {
    fn drop(&mut self) {
        self.close();
    }
}

#[async_trait]
impl FileIOBackend for Backend {
    async fn request(
        &self,
        request: FileIORequest,
        cancel: CancellationToken,
    ) -> Result<FileIOResponse, FileIOError> {
        let inner = self
            .0
            .upgrade()
            .ok_or_else(|| error(FileIOErrorCode::Disconnected))?;
        // Deadline starts before validation/admission, and includes enqueue.
        let deadline = tokio::time::Instant::now() + inner.timeout;
        if cancel.is_cancelled() {
            return Err(error(FileIOErrorCode::Cancelled));
        }
        let (mut event, expected) = encode_request(request)?;
        let (request, writer, closure) = {
            let mut state = inner.gate.lock().expect("file I/O gate poisoned");
            if state.closed || inner.stop.is_cancelled() {
                return Err(error(FileIOErrorCode::Disconnected));
            }
            let writer = state
                .writer
                .as_ref()
                .ok_or_else(|| error(FileIOErrorCode::Disconnected))?
                .clone();
            if writer.failure().is_cancelled() {
                return Err(error(FileIOErrorCode::Disconnected));
            }
            if cancel.is_cancelled() {
                return Err(error(FileIOErrorCode::Cancelled));
            }
            let request = inner
                .broker
                .register()
                .map_err(|_| error(FileIOErrorCode::Unsupported))?;
            let closure = Arc::new(Closure {
                writer: writer.clone(),
                id: request.id.clone(),
                published: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            });
            state.pending.insert(
                request.id.clone(),
                Pending {
                    expected,
                    closure: closure.clone(),
                },
            );
            (request, writer, closure)
        };
        event["requestId"] = json!(request.id);
        // Only the outer select owns cancellation/deadline. Letting wait()'s
        // own cancellation win could release its slot before RAII closure.
        let never_cancel = CancellationToken::new();
        let waiter = request.wait(inner.timeout + Duration::from_secs(1), &never_cancel);
        tokio::pin!(waiter);
        // Drop order is intentional; see CloseRequest. All awaits below borrow
        // waiter, so an aborted caller cannot drop the broker request first.
        let close = CloseRequest {
            inner: inner.clone(),
            closure: closure.clone(),
        };
        let failure = writer.failure();
        let operation = async {
            if !fits_payload(&event) {
                return Err(error(FileIOErrorCode::TooLarge));
            }
            // Size is checked before allocating the encoded buffer. No JSON
            // serialization or asynchronous wait occurs under the state gate.
            let mut line =
                serde_json::to_vec(&event).map_err(|_| error(FileIOErrorCode::InvalidResponse))?;
            line.push(b'\n');
            let permit = writer
                .reserve_bounded()
                .await
                .map_err(|_| error(FileIOErrorCode::Disconnected))?;
            #[cfg(test)]
            {
                let hook = inner
                    .before_publish
                    .lock()
                    .expect("file I/O publication hook poisoned")
                    .take();
                if let Some(hook) = hook {
                    hook();
                }
            }
            {
                let state = inner.gate.lock().expect("file I/O gate poisoned");
                if state.closed || inner.stop.is_cancelled() || failure.is_cancelled() {
                    return Err(error(FileIOErrorCode::Disconnected));
                }
                if cancel.is_cancelled() {
                    return Err(error(FileIOErrorCode::Cancelled));
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(error(FileIOErrorCode::Timeout));
                }
                // A known early response may already have retired this ID.
                // Consume that response below, but never publish it afterwards.
                if state.pending.contains_key(&closure.id)
                    && !closure.closed.load(Ordering::Acquire)
                {
                    // The lifecycle gate orders this synchronous commit and
                    // publication marker against every response/close/drop.
                    permit.send(line);
                    closure.published.store(true, Ordering::Release);
                } else {
                    drop(permit);
                }
            }
            (&mut waiter)
                .await
                .map_err(|message| match message.as_str() {
                    "client request cancelled" => error(FileIOErrorCode::Cancelled),
                    "client request timed out" => error(FileIOErrorCode::Timeout),
                    _ => error(FileIOErrorCode::Disconnected),
                })
        };
        let reply = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(error(FileIOErrorCode::Cancelled)),
            _ = inner.stop.cancelled() => Err(error(FileIOErrorCode::Disconnected)),
            _ = failure.cancelled() => Err(error(FileIOErrorCode::Disconnected)),
            _ = tokio::time::sleep_until(deadline) => Err(error(FileIOErrorCode::Timeout)),
            reply = operation => reply,
        };
        close.close();
        if failure.is_cancelled() || inner.stop.is_cancelled() {
            return Err(error(FileIOErrorCode::Disconnected));
        }
        let reply = reply?;
        if let Some(remote) = reply.get("error") {
            // This envelope is exclusively generated by resolve_response after
            // validation, never decoded from arbitrary broker error strings.
            let code = remote["code"]
                .as_str()
                .and_then(parse_code)
                .ok_or_else(|| error(FileIOErrorCode::InvalidResponse))?;
            let message = remote["message"]
                .as_str()
                .ok_or_else(|| error(FileIOErrorCode::InvalidResponse))?;
            return Err(FileIOError::new(code, message));
        }
        match expected {
            Expected::Read { .. } => Ok(FileIOResponse::Text {
                content: reply["result"]["content"]
                    .as_str()
                    .ok_or_else(|| error(FileIOErrorCode::InvalidResponse))?
                    .to_owned(),
            }),
            Expected::Write => Ok(FileIOResponse::Written),
        }
    }
}

#[cfg(test)]
#[path = "file_io_tests.rs"]
mod tests;
