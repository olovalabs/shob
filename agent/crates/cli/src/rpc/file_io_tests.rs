//! Foundation tests only: no provider, authentication, transport dispatcher or
//! harness. Parent must wire the module and run these (not executed by author).
use super::super::writer::{start_rpc_writer, write_line};
use super::*;
use std::future::Future;
use std::task::{Context, Poll};
use tokio::{sync::mpsc, task::JoinHandle};

type ResponseTask = JoinHandle<Result<FileIOResponse, FileIOError>>;

struct Memory(mpsc::Sender<Value>);
impl Write for Memory {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let value = serde_json::from_slice(bytes).map_err(io::Error::other)?;
        self.0.try_send(value).map_err(io::Error::other)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Fixture {
    client: Option<FileIOClient>,
    writer: Option<RpcWriter>,
    frames: mpsc::Receiver<Value>,
    join: JoinHandle<io::Result<()>>,
    failure: CancellationToken,
}
impl Fixture {
    fn new() -> Self {
        Self::timeout(Duration::from_secs(60))
    }
    fn timeout(timeout: Duration) -> Self {
        let (tx, frames) = mpsc::channel(256);
        let (writer, join, failure) = start_rpc_writer(Memory(tx), Default::default());
        let client = FileIOClient::with_timeout(writer.clone(), timeout);
        Self {
            client: Some(client),
            writer: Some(writer),
            frames,
            join,
            failure,
        }
    }
    fn client(&self) -> &FileIOClient {
        self.client.as_ref().unwrap()
    }
    fn backend(&self) -> Arc<dyn FileIOBackend> {
        self.client().backend()
    }
    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(3), self.frames.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn start(&mut self, request: FileIORequest) -> (ResponseTask, Value) {
        let backend = self.backend();
        let task =
            tokio::spawn(async move { backend.request(request, CancellationToken::new()).await });
        let event = self.next().await;
        assert_eq!(event["type"], "file_io_request");
        (task, event)
    }
    async fn closed(&mut self, id: &Value) {
        assert_eq!(
            self.next().await,
            json!({"type":"file_io_request_closed", "requestId":id})
        );
    }
    async fn no_frames(&mut self) {
        // FIFO fence: inspect actual writer output, not just the receiver
        // before the blocking writer has had a chance to drain its queue.
        let fence = json!({"test": "file_io_writer_fence"});
        write_line(self.writer.as_ref().unwrap(), &fence).await;
        assert_eq!(self.next().await, fence);
        assert!(self.frames.is_empty());
    }
    async fn finish(mut self, failed: bool) {
        drop(self.client.take());
        drop(self.writer.take());
        let outcome = tokio::time::timeout(Duration::from_secs(3), self.join)
            .await
            .unwrap()
            .unwrap();
        if failed {
            assert!(
                outcome.is_err(),
                "failed writer must not masquerade as clean shutdown"
            );
        } else {
            assert!(
                outcome.is_ok(),
                "healthy writer must drain and join cleanly: {outcome:?}"
            );
        }
    }
}
fn read(range: FileIOReadRange) -> FileIORequest {
    FileIORequest::ReadText {
        absolute_path: "/workspace/a/../file.txt".into(),
        range,
        max_bytes: MAX_FILE_IO_BYTES,
    }
}
fn complete() -> FileIORequest {
    read(FileIOReadRange::Complete)
}
fn write(content: &str) -> FileIORequest {
    FileIORequest::WriteText {
        absolute_path: "C:\\work\\a\\..\\file.txt".into(),
        content: content.into(),
    }
}
fn response(id: &Value, result: Value) -> Value {
    json!({"type":"file_io_response", "requestId":id, "result":result})
}
async fn result(task: ResponseTask) -> Result<FileIOResponse, FileIOError> {
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
}

// These callbacks run after reservation in the same backend poll. Unlike an
// async barrier, they cannot let select's next cancellation check hide a missing
// publication-gate check. No sleeps, retry loops, or stand-alone model state.
#[tokio::test]
async fn close_between_reservation_and_send_never_publishes_or_closes_unpublished() {
    for owner_drop in [false, true] {
        let mut f = Fixture::new();
        let backend = f.backend();
        let client = f.client.take().unwrap();
        let inner = client.inner.clone();
        let observed = inner.clone();
        *inner.before_publish.lock().unwrap() = Some(Box::new(move || {
            let closure = {
                let state = observed.gate.lock().unwrap();
                assert_eq!(state.pending.len(), 1);
                state.pending.values().next().unwrap().closure.clone()
            };
            assert!(!closure.published.load(Ordering::Acquire));
            if owner_drop {
                drop(client);
            } else {
                client.close();
                client.close();
            }
            assert!(closure.closed.load(Ordering::Acquire));
            assert!(!closure.published.load(Ordering::Acquire));
            assert!(observed.gate.lock().unwrap().pending.is_empty());
        }));
        assert_eq!(
            backend
                .request(
                    write("must not commit after owner close"),
                    CancellationToken::new()
                )
                .await
                .unwrap_err()
                .code,
            FileIOErrorCode::Disconnected
        );
        assert!(inner.before_publish.lock().unwrap().is_none(), "hook ran");
        assert!(inner.gate.lock().unwrap().pending.is_empty());
        f.no_frames().await;
        drop(inner);
        drop(backend);
        f.finish(false).await;
    }
}

#[tokio::test]
async fn early_known_reply_retires_unpublished_and_suppresses_later_send() {
    for valid in [false, true] {
        let mut f = Fixture::new();
        let client = Arc::new(f.client.take().unwrap());
        let backend = client.backend();
        let responder = client.clone();
        *client.inner.before_publish.lock().unwrap() = Some(Box::new(move || {
            let closure = {
                let state = responder.inner.gate.lock().unwrap();
                assert_eq!(state.pending.len(), 1);
                state.pending.values().next().unwrap().closure.clone()
            };
            let reply = response(
                &json!(closure.id),
                if valid { json!({}) } else { Value::Null },
            );
            assert!(responder.resolve_response(&reply));
            assert!(!responder.resolve_response(&reply));
            assert!(closure.closed.load(Ordering::Acquire));
            assert!(!closure.published.load(Ordering::Acquire));
            assert!(responder.inner.broker.register().is_ok());
        }));
        let outcome = backend
            .request(write("not dispatched"), CancellationToken::new())
            .await;
        if valid {
            assert_eq!(outcome.unwrap(), FileIOResponse::Written);
        } else {
            assert_eq!(outcome.unwrap_err().code, FileIOErrorCode::InvalidResponse);
        }
        assert!(client.inner.before_publish.lock().unwrap().is_none());
        assert!(client.inner.gate.lock().unwrap().pending.is_empty());
        f.no_frames().await;
        drop(client);
        drop(backend);
        f.finish(false).await;
    }
}

#[tokio::test]
async fn cancellation_and_failure_after_reservation_prevent_publication() {
    let mut f = Fixture::new();
    let cancel = CancellationToken::new();
    let hook_cancel = cancel.clone();
    *f.client().inner.before_publish.lock().unwrap() = Some(Box::new(move || hook_cancel.cancel()));
    assert_eq!(
        f.backend()
            .request(write("cancelled"), cancel)
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::Cancelled
    );
    assert!(f.client().inner.before_publish.lock().unwrap().is_none());
    assert!(f.client().inner.gate.lock().unwrap().pending.is_empty());
    f.no_frames().await;
    f.finish(false).await;

    let f = Fixture::new();
    let failure = f.failure.clone();
    *f.client().inner.before_publish.lock().unwrap() = Some(Box::new(move || failure.cancel()));
    assert_eq!(
        f.backend()
            .request(write("failed"), CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::Disconnected
    );
    assert!(f.client().inner.before_publish.lock().unwrap().is_none());
    assert!(f.client().inner.gate.lock().unwrap().pending.is_empty());
    assert!(f.frames.is_empty());
    f.finish(true).await;
}

#[tokio::test]
async fn published_request_precedes_exactly_one_closed_notification() {
    let mut f = Fixture::new();
    let backend = f.backend();
    let mut request = Box::pin(backend.request(write("published"), CancellationToken::new()));
    assert!(matches!(
        request
            .as_mut()
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref())),
        Poll::Pending
    ));
    // Neither frame has been consumed: verify the actual writer's FIFO order.
    let closure = {
        let state = f.client().inner.gate.lock().unwrap();
        assert_eq!(state.pending.len(), 1);
        state.pending.values().next().unwrap().closure.clone()
    };
    assert!(closure.published.load(Ordering::Acquire));
    f.client().close();
    f.client().close();
    assert_eq!(
        request.await.unwrap_err().code,
        FileIOErrorCode::Disconnected
    );
    let event = f.next().await;
    assert_eq!(event["type"], "file_io_request");
    assert_eq!(event["requestId"], json!(closure.id));
    f.closed(&event["requestId"]).await;
    assert!(closure.closed.load(Ordering::Acquire));
    f.no_frames().await;
    drop(backend);
    drop(closure);
    f.finish(false).await;
}

#[tokio::test]
async fn exact_read_ranges_complete_and_write_wire_including_empty_results() {
    let mut f = Fixture::new();
    for range in [
        FileIOReadRange::Lines {
            offset_1based: 3,
            limit: 7,
        },
        FileIOReadRange::Complete,
    ] {
        let (task, event) = f.start(read(range)).await;
        let id = &event["requestId"];
        let native = id.as_str().unwrap().strip_prefix("native:").unwrap();
        assert!(uuid::Uuid::parse_str(native).is_ok());
        let mut expected = json!({"type":"file_io_request", "requestId":id,
            "operation":"read_text_file", "path":"/workspace/file.txt", "maxBytes":262144});
        if let FileIOReadRange::Lines {
            offset_1based,
            limit,
        } = range
        {
            expected["line"] = json!(offset_1based);
            expected["limit"] = json!(limit);
        }
        assert_eq!(event, expected);
        assert!(f
            .client()
            .resolve_response(&response(id, json!({"content":""}))));
        assert_eq!(
            result(task).await.unwrap(),
            FileIOResponse::Text {
                content: String::new()
            }
        );
        f.closed(id).await;
    }
    for content in ["", "some text\n"] {
        let (task, event) = f.start(write(content)).await;
        let id = &event["requestId"];
        assert_eq!(
            event,
            json!({"type":"file_io_request", "requestId":id,
            "operation":"write_text_file", "path":"C:/work/file.txt", "content":content})
        );
        assert!(f.client().resolve_response(&response(id, json!({}))));
        assert_eq!(result(task).await.unwrap(), FileIOResponse::Written);
        f.closed(id).await;
    }
    f.finish(false).await;
}

#[tokio::test]
async fn known_malformed_replies_retire_and_never_echo_sensitive_data() {
    let mut f = Fixture::new();
    let malformed = [
        json!({"result":null}),
        json!({"result":{"content":1}}),
        json!({"result":{"content":"secret", "path":"private"}}),
        json!({"result":{"content":"secret"}, "error":{"code":"denied", "message":"private"}}),
        json!({"error":{"code":"made_up_secret", "message":"private"}}),
        json!({"error":"private"}),
        json!({"cancelled":true}),
        json!({}),
        json!({"error":{"code":"denied", "message":"x".repeat(MAX_ERROR_BYTES + 1)}}),
        json!({"error":{"code":"denied", "message":"private", "extra":true}}),
        json!({"result":{"content":"secret"}, "metadata":"private"}),
    ];
    for fields in malformed {
        let (task, event) = f.start(complete()).await;
        let id = &event["requestId"];
        let mut reply = fields;
        reply["type"] = json!("file_io_response");
        reply["requestId"] = id.clone();
        assert!(f.client().resolve_response(&reply));
        assert!(
            !f.client().resolve_response(&reply),
            "duplicate is already retired"
        );
        let error = result(task).await.unwrap_err();
        assert_eq!(error.code, FileIOErrorCode::InvalidResponse);
        assert_eq!(error.message, "invalid client file I/O response");
        f.closed(id).await;
        assert!(!f
            .client()
            .resolve_response(&response(id, json!({"content":"late"}))));
    }
    // Write acknowledgments must be explicit empty objects, never null/text.
    for ack in [Value::Null, json!({"content":""}), json!(true)] {
        let (task, event) = f.start(write("")).await;
        assert!(f
            .client()
            .resolve_response(&response(&event["requestId"], ack)));
        assert_eq!(
            result(task).await.unwrap_err().code,
            FileIOErrorCode::InvalidResponse
        );
        f.closed(&event["requestId"]).await;
    }
    // A known ID with a wrong envelope type also settles, not hangs.
    let (task, event) = f.start(complete()).await;
    let mut reply = response(&event["requestId"], json!({"content":""}));
    reply["type"] = json!("wrong");
    assert!(f.client().resolve_response(&reply));
    assert_eq!(
        result(task).await.unwrap_err().code,
        FileIOErrorCode::InvalidResponse
    );
    f.closed(&event["requestId"]).await;
    assert!(!f
        .client()
        .resolve_response(&json!({"requestId":5, "result":{}})));
    assert!(!f.client().resolve_response(&json!({"result":{}})));
    assert!(!f.client().resolve_response(&response(
        &json!("native:unknown"),
        json!({"content":"x".repeat(MAX_PAYLOAD_BYTES + 1)})
    )));
    f.finish(false).await;
}

#[tokio::test]
async fn fixed_remote_codes_and_bounded_message_are_preserved() {
    let mut f = Fixture::new();
    for code in [
        "unsupported",
        "denied",
        "not_found",
        "invalid_response",
        "too_large",
        "timeout",
        "cancelled",
        "disconnected",
        "invalid_path",
    ] {
        let (task, event) = f.start(complete()).await;
        let message = "é".repeat(MAX_ERROR_BYTES / 2);
        let reply = json!({"type":"file_io_response", "requestId":event["requestId"],
            "error":{"code":code, "message":message}});
        assert!(f.client().resolve_response(&reply));
        let error = result(task).await.unwrap_err();
        assert_eq!(Some(error.code), parse_code(code));
        assert_eq!(error.message, message);
        f.closed(&event["requestId"]).await;
    }
    f.finish(false).await;
}

#[tokio::test]
async fn utf8_and_encoded_escaping_budgets_are_independent() {
    let mut f = Fixture::new();
    // UTF-8 boundary succeeds; escape-heavy 256 KiB fails the encoded budget.
    for (content, valid) in [
        ("é".repeat(MAX_FILE_IO_BYTES / 2), true),
        ("a".repeat(MAX_FILE_IO_BYTES + 1), false),
        ("\u{0001}".repeat(MAX_FILE_IO_BYTES), false),
    ] {
        let (task, event) = f.start(complete()).await;
        let reply = response(&event["requestId"], json!({"content":content}));
        assert!(f.client().resolve_response(&reply));
        let outcome = result(task).await;
        if valid {
            assert_eq!(outcome.unwrap(), FileIOResponse::Text { content });
        } else {
            assert_eq!(outcome.unwrap_err().code, FileIOErrorCode::InvalidResponse);
        }
        f.closed(&event["requestId"]).await;
    }
    let backend = f.backend();
    assert_eq!(
        backend
            .request(
                write(&"a".repeat(MAX_FILE_IO_BYTES + 1)),
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::TooLarge
    );
    // Encoded outgoing event is independently capped, without emitting request.
    assert_eq!(
        backend
            .request(
                write(&"\u{0001}".repeat(MAX_FILE_IO_BYTES)),
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::TooLarge
    );
    f.no_frames().await;
    assert!(f.client().inner.gate.lock().unwrap().pending.is_empty());
    f.finish(false).await;
}

#[tokio::test]
async fn normalization_limits_and_invalid_requests_emit_nothing() {
    let f = Fixture::new();
    for path in [
        "relative",
        "C:relative",
        "\\root_relative",
        "file://private",
        "~/.secret",
        "\\\\?\\C:\\private",
    ] {
        let request = FileIORequest::ReadText {
            absolute_path: path.into(),
            range: FileIOReadRange::Complete,
            max_bytes: MAX_FILE_IO_BYTES,
        };
        assert_eq!(
            f.backend()
                .request(request, CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            FileIOErrorCode::InvalidPath
        );
    }
    for range in [
        FileIOReadRange::Lines {
            offset_1based: 0,
            limit: 1,
        },
        FileIOReadRange::Lines {
            offset_1based: 1,
            limit: 0,
        },
    ] {
        assert_eq!(
            f.backend()
                .request(read(range), CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            FileIOErrorCode::InvalidResponse
        );
    }
    assert!(f.client().inner.gate.lock().unwrap().pending.is_empty());
    assert!(f.frames.is_empty());
    f.finish(false).await;
}

#[tokio::test]
async fn sixty_four_slots_include_resolved_but_unconsumed_and_close_precedes_release() {
    let mut f = Fixture::new();
    let mut requests = Vec::new();
    // Manually poll to hold resolved futures unconsumed, without scheduler races.
    for _ in 0..64 {
        let backend = f.backend();
        let mut future =
            Box::pin(async move { backend.request(complete(), CancellationToken::new()).await });
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(futures::task::noop_waker_ref())),
            Poll::Pending
        ));
        requests.push(future);
    }
    let mut ids = Vec::new();
    for _ in 0..64 {
        ids.push(f.next().await["requestId"].clone());
    }
    assert_eq!(f.client().inner.gate.lock().unwrap().pending.len(), 64);
    assert_eq!(
        f.backend()
            .request(complete(), CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::Unsupported
    );
    assert!(f
        .client()
        .resolve_response(&response(&ids[0], json!({"content":""}))));
    assert!(
        f.client().inner.broker.register().is_err(),
        "response must not prematurely release capacity"
    );
    f.closed(&ids[0]).await;
    assert!(!f
        .client()
        .resolve_response(&response(&ids[0], json!({"content":"late"}))));
    // Closure is already observable while the waiter still owns its slot.
    assert!(matches!(
        requests[0]
            .as_mut()
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref())),
        Poll::Ready(Ok(_))
    ));
    drop(f.client().inner.broker.register().unwrap());
    // Drop each remaining caller, then observe closure before testing slot reuse.
    // Drain individually to avoid overflowing the intentionally bounded writer.
    drop(requests.remove(0));
    for (request, id) in requests.into_iter().zip(ids.iter().skip(1)) {
        drop(request);
        f.closed(id).await;
        assert!(!f
            .client()
            .resolve_response(&response(id, json!({"content":"late"}))));
    }
    assert!(f.client().inner.gate.lock().unwrap().pending.is_empty());
    let slots: Vec<_> = (0..64)
        .map(|_| f.client().inner.broker.register().unwrap())
        .collect();
    assert!(f.client().inner.broker.register().is_err());
    drop(slots);
    f.finish(false).await;
}

#[tokio::test]
async fn pre_cancel_inflight_cancel_timeout_and_caller_abort_retire() {
    let mut f = Fixture::timeout(Duration::from_millis(30));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        f.backend()
            .request(complete(), cancel)
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::Cancelled
    );
    assert!(f.frames.is_empty());
    let (task, event) = f.start(complete()).await;
    assert_eq!(
        result(task).await.unwrap_err().code,
        FileIOErrorCode::Timeout
    );
    f.closed(&event["requestId"]).await;
    assert!(!f
        .client()
        .resolve_response(&response(&event["requestId"], json!({"content":"late"}))));
    f.finish(false).await;

    let mut f = Fixture::new();
    let cancel = CancellationToken::new();
    let backend = f.backend();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move { backend.request(complete(), task_cancel).await });
    let event = f.next().await;
    cancel.cancel();
    assert_eq!(
        result(task).await.unwrap_err().code,
        FileIOErrorCode::Cancelled
    );
    f.closed(&event["requestId"]).await;
    assert!(!f
        .client()
        .resolve_response(&response(&event["requestId"], json!({"content":"late"}))));
    let (task, event) = f.start(complete()).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    f.closed(&event["requestId"]).await;
    assert!(!f
        .client()
        .resolve_response(&response(&event["requestId"], json!({"content":"late"}))));
    assert!(f.client().inner.gate.lock().unwrap().pending.is_empty());
    f.finish(false).await;
}

#[tokio::test]
async fn explicit_close_and_owner_drop_wake_inflight_even_with_inner_alive() {
    for owner_drop in [false, true] {
        let mut f = Fixture::new();
        let backend = f.backend();
        let weak = Arc::downgrade(&f.client().inner);
        let (task, event) = f.start(complete()).await;
        // Prove that close is tied to the unique owner, not Inner's final drop.
        let held_inner = weak.upgrade().unwrap();
        if owner_drop {
            drop(f.client.take());
        } else {
            f.client().close();
            f.client().close();
        }
        assert_eq!(
            result(task).await.unwrap_err().code,
            FileIOErrorCode::Disconnected
        );
        f.closed(&event["requestId"]).await;
        assert_eq!(
            backend
                .request(complete(), CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            FileIOErrorCode::Disconnected
        );
        assert!(held_inner.gate.lock().unwrap().pending.is_empty());
        assert!(!held_inner
            .broker
            .resolve(event["requestId"].as_str().unwrap(), Ok(Value::Null)));
        drop(held_inner);
        drop(f.client.take());
        assert!(
            weak.upgrade().is_none(),
            "backend must own only Weak<Inner>"
        );
        f.finish(false).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_racing_registration_never_admits_after_shutdown() {
    let f = Fixture::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let backend = f.backend();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            backend.request(complete(), CancellationToken::new()).await
        }));
    }
    barrier.wait().await;
    f.client().close();
    for task in tasks {
        assert_eq!(
            result(task).await.unwrap_err().code,
            FileIOErrorCode::Disconnected
        );
    }
    for _ in 0..100 {
        assert_eq!(
            f.backend()
                .request(complete(), CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            FileIOErrorCode::Disconnected
        );
        assert!(f.client().inner.gate.lock().unwrap().pending.is_empty());
    }
    // At most eight requests and eight closures; shutdown drains and joins.
    f.finish(false).await;
}

#[tokio::test]
async fn transport_failure_wakes_pending_and_rejects_further_admission() {
    let mut f = Fixture::new();
    let (task, event) = f.start(complete()).await;
    f.failure.cancel();
    assert_eq!(
        result(task).await.unwrap_err().code,
        FileIOErrorCode::Disconnected
    );
    assert!(!f
        .client()
        .resolve_response(&response(&event["requestId"], json!({"content":"late"}))));
    assert_eq!(
        f.backend()
            .request(complete(), CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::Disconnected
    );
    f.finish(true).await;
}

struct Blocked {
    entered: Option<std::sync::mpsc::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}
impl Write for Blocked {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
            let _ = self.release.recv();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
async fn blocked_writer(
    queued: usize,
) -> (
    RpcWriter,
    JoinHandle<io::Result<()>>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, received) = std::sync::mpsc::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let (writer, join, _) = start_rpc_writer(
        Blocked {
            entered: Some(entered),
            release: gate,
        },
        Default::default(),
    );
    write_line(&writer, &json!("blocking")).await;
    tokio::task::spawn_blocking(move || received.recv().unwrap())
        .await
        .unwrap();
    for n in 0..queued {
        write_line(&writer, &json!(n)).await;
    }
    (writer, join, release)
}

#[tokio::test]
async fn dropping_partial_reservation_has_no_io_side_effects_or_slot_leak() {
    let (writer, join, release) = blocked_writer(64).await;
    let client = FileIOClient::new(writer.clone());
    let backend = client.backend();
    let mut request = Box::pin(backend.request(write("never queued"), CancellationToken::new()));
    let polled = request
        .as_mut()
        .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));
    let closure = client
        .inner
        .gate
        .lock()
        .unwrap()
        .pending
        .values()
        .next()
        .map(|pending| pending.closure.clone());
    drop(request);
    // Release the OS writer before any assertion can unwind this test.
    release.send(()).unwrap();
    assert!(matches!(polled, Poll::Pending));
    let closure = closure.expect("registered before waiting for queue capacity");
    assert!(closure.closed.load(Ordering::Acquire));
    assert!(!closure.published.load(Ordering::Acquire));
    assert!(
        !writer.failure().is_cancelled(),
        "no unpublished closure enqueue"
    );
    assert!(client.inner.gate.lock().unwrap().pending.is_empty());
    let slots: Vec<_> = (0..64)
        .map(|_| client.inner.broker.register().unwrap())
        .collect();
    assert!(client.inner.broker.register().is_err());
    drop(slots);
    drop(closure);
    drop(client);
    drop(writer);
    assert!(tokio::time::timeout(Duration::from_secs(3), join)
        .await
        .unwrap()
        .unwrap()
        .is_ok());
}

#[tokio::test]
async fn enqueue_deadline_failure_and_closure_overflow_are_authoritative() {
    let (writer, join, release) = blocked_writer(64).await;
    let client = FileIOClient::new(writer.clone());
    let backend = client.backend();
    let outcome = backend.request(complete(), CancellationToken::new()).await;
    // Always release OS I/O before assertions, including failing assertions.
    release.send(()).unwrap();
    assert_eq!(outcome.unwrap_err().code, FileIOErrorCode::Disconnected);
    assert!(writer.failure().is_cancelled());
    assert!(client.inner.gate.lock().unwrap().pending.is_empty());
    drop(client);
    drop(writer);
    assert!(tokio::time::timeout(Duration::from_secs(3), join)
        .await
        .unwrap()
        .unwrap()
        .is_err());

    // The shorter TOTAL deadline includes reservation. An unpublished request
    // has no closure to enqueue, so cancellation leaves the writer healthy.
    let (writer, join, release) = blocked_writer(64).await;
    let client = FileIOClient::with_timeout(writer.clone(), Duration::from_millis(10));
    let outcome = client
        .backend()
        .request(complete(), CancellationToken::new())
        .await;
    release.send(()).unwrap();
    assert_eq!(outcome.unwrap_err().code, FileIOErrorCode::Timeout);
    assert!(!writer.failure().is_cancelled());
    assert!(client.inner.gate.lock().unwrap().pending.is_empty());
    drop(client);
    drop(writer);
    assert!(tokio::time::timeout(Duration::from_secs(3), join)
        .await
        .unwrap()
        .unwrap()
        .is_ok());

    // A published request still requires a closure: a full queue makes failure
    // authoritative rather than silently losing the client's notification.
    let (writer, join, release) = blocked_writer(63).await;
    let client = FileIOClient::new(writer.clone());
    let backend = client.backend();
    let mut request = Box::pin(backend.request(complete(), CancellationToken::new()));
    let polled = request
        .as_mut()
        .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));
    let published = {
        let state = client.inner.gate.lock().unwrap();
        state.pending.len() == 1
            && state
                .pending
                .values()
                .all(|pending| pending.closure.published.load(Ordering::Acquire))
    };
    client.close();
    let outcome = request.await;
    release.send(()).unwrap();
    assert!(matches!(polled, Poll::Pending));
    assert!(published);
    assert_eq!(outcome.unwrap_err().code, FileIOErrorCode::Disconnected);
    assert!(writer.failure().is_cancelled());
    assert!(client.inner.gate.lock().unwrap().pending.is_empty());
    drop(client);
    drop(writer);
    assert!(tokio::time::timeout(Duration::from_secs(3), join)
        .await
        .unwrap()
        .unwrap()
        .is_err());
}
