//! Ordered, bounded PTY input. Only the dedicated worker touches the physical writer.
use std::io::{self, Write};
use std::sync::Arc;

use parking_lot::Mutex;

const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_INPUT_RECORDS: usize = 4096;
const FULL: &str = "PTY input queue is full; input was not accepted. Wait for the command to read input, then retry.";

/// Acknowledges a complete physical write, not merely admission to the input queue.
/// A failed write is never replayed, including when part of its frame reached the PTY.
pub struct WriteCompletion {
    receiver: Option<flume::Receiver<Result<(), String>>>,
}

impl WriteCompletion {
    pub async fn wait(self) -> Result<(), String> {
        match self.receiver {
            Some(receiver) => receiver.recv_async().await.unwrap_or_else(|_| {
                Err("PTY input worker stopped before completing the write".into())
            }),
            None => Ok(()),
        }
    }
}

#[derive(Default)]
struct Outstanding {
    bytes: usize,
    records: usize,
    failed: Option<String>,
    error: Option<String>,
}

struct Shared {
    outstanding: Mutex<Outstanding>,
    errors: flume::Sender<()>,
}

impl Shared {
    fn fail(&self, message: String) {
        let mut outstanding = self.outstanding.lock();
        if outstanding.failed.is_none() {
            outstanding.failed = Some(message.clone());
            outstanding.error = Some(message);
            let _ = self.errors.try_send(());
        }
    }

    fn report_error(&self, message: String) {
        let mut outstanding = self.outstanding.lock();
        // Never overwrite a permanent failure with a racing capacity warning.
        if outstanding.failed.is_none() {
            outstanding.error = Some(message);
            let _ = self.errors.try_send(());
        }
    }
}

/// One bounded wakeup plus the latest diagnostic: refusal storms cannot grow
/// memory, and a permanent worker failure replaces any pending capacity warning.
pub(crate) struct InputErrors {
    receiver: flume::Receiver<()>,
    shared: Arc<Shared>,
}

impl InputErrors {
    pub(crate) async fn recv_async(&self) -> Result<String, flume::RecvError> {
        loop {
            self.receiver.recv_async().await?;
            if let Some(message) = self.shared.outstanding.lock().error.take() {
                return Ok(message);
            }
        }
    }

    #[cfg(test)]
    fn recv(&self) -> Result<String, flume::RecvError> {
        loop {
            self.receiver.recv()?;
            if let Some(message) = self.shared.outstanding.lock().error.take() {
                return Ok(message);
            }
        }
    }

    #[cfg(test)]
    fn try_recv(&self) -> Result<String, flume::TryRecvError> {
        loop {
            self.receiver.try_recv()?;
            if let Some(message) = self.shared.outstanding.lock().error.take() {
                return Ok(message);
            }
        }
    }
}

struct Pending {
    bytes: Vec<u8>,
    shared: Arc<Shared>,
    completion: Option<flume::Sender<Result<(), String>>>,
    counted: bool,
}

impl Pending {
    fn release(&mut self) {
        if self.counted {
            let mut outstanding = self.shared.outstanding.lock();
            outstanding.bytes -= self.bytes.len();
            outstanding.records -= 1;
            self.counted = false;
        }
    }

    fn complete(mut self, result: Result<(), String>) {
        // Release capacity before waking a caller that may immediately send again.
        self.release();
        if let Some(completion) = self.completion.take() {
            let _ = completion.try_send(result);
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.release();
        if let Some(completion) = self.completion.take() {
            let message = self
                .shared
                .outstanding
                .lock()
                .failed
                .clone()
                .unwrap_or_else(|| "PTY input worker stopped before completing the write".into());
            let _ = completion.try_send(Err(message));
        }
    }
}

/// Clones share one FIFO and bounds that include the write currently in flight.
#[derive(Clone)]
pub(crate) struct QueuedWriter {
    sender: flume::Sender<Pending>,
    shared: Arc<Shared>,
    max_bytes: usize,
    max_records: usize,
}

impl QueuedWriter {
    pub(crate) fn new(
        writer: impl Write + Send + 'static,
        executor: &gpui::BackgroundExecutor,
    ) -> (Self, InputErrors) {
        let (queued, errors, start) = Self::prepare(writer, MAX_INPUT_BYTES, MAX_INPUT_RECORDS);
        // Even a failed thread spawn drops its captured writer. Do the spawn on
        // the existing pool so this failure path cannot run a PTY destructor on GPUI.
        executor
            .spawn(async move {
                start();
            })
            .detach();
        (queued, errors)
    }

    fn prepare(
        writer: impl Write + Send + 'static,
        max_bytes: usize,
        max_records: usize,
    ) -> (Self, InputErrors, impl FnOnce() + Send + 'static) {
        let (sender, receiver) = flume::bounded::<Pending>(max_records);
        let (errors, error_rx) = flume::bounded(1);
        let shared = Arc::new(Shared {
            outstanding: Mutex::new(Outstanding::default()),
            errors,
        });
        let error_rx = InputErrors {
            receiver: error_rx,
            shared: shared.clone(),
        };
        let queued = Self {
            sender,
            shared: shared.clone(),
            max_bytes,
            max_records,
        };
        let start = move || {
            let worker_shared = shared.clone();
            // Retain ownership until spawn succeeds, so a spawn failure can
            // publish its diagnostic before a potentially blocking writer Drop.
            let failed_receiver = receiver.clone();
            let writer = Arc::new(Mutex::new(Some(writer)));
            let worker_writer = writer.clone();
            let result = std::thread::Builder::new()
                .name("pty-input".into())
                .spawn(move || {
                    let mut writer = worker_writer.lock().take().expect("sole PTY writer worker");
                    while let Ok(pending) = receiver.recv() {
                        match writer.write_all(&pending.bytes) {
                            Ok(()) => pending.complete(Ok(())),
                            Err(error) => {
                                let message = format!("PTY input failed: {error}");
                                worker_shared.fail(message.clone());
                                pending.complete(Err(message.clone()));
                                // Dropping a flume receiver alone can retain its
                                // queued values while senders still exist.
                                reject_queued(&receiver, &message);
                                // Dropping the queue rejects all later receipts. Never
                                // retry a frame whose prefix may already have been written.
                                break;
                            }
                        }
                    }
                    // Resolve queued receipts before a blocking physical destructor.
                    drop(receiver);
                    // The physical writer (including blocking Drop) stays on this worker.
                });
            if let Err(error) = result {
                let message = format!("Cannot start PTY input worker: {error}");
                shared.fail(message.clone());
                reject_queued(&failed_receiver, &message);
            }
        };
        (queued, error_rx, start)
    }

    fn reject(&self, kind: io::ErrorKind, message: &str) -> io::Error {
        self.shared.report_error(message.to_owned());
        io::Error::new(kind, message)
    }

    fn enqueue(&self, bytes: &[u8], acknowledge: bool) -> io::Result<WriteCompletion> {
        let mut outstanding = self.shared.outstanding.lock();
        if let Some(message) = &outstanding.failed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, message.clone()));
        }
        if bytes.len() > self.max_bytes {
            drop(outstanding);
            return Err(self.reject(
                io::ErrorKind::InvalidInput,
                "PTY input is too large; the entire input was refused (maximum 32 MiB per frame).",
            ));
        }
        if bytes.is_empty() {
            return Ok(WriteCompletion { receiver: None });
        }
        if outstanding.records >= self.max_records
            || outstanding.bytes > self.max_bytes - bytes.len()
        {
            drop(outstanding);
            return Err(self.reject(io::ErrorKind::WouldBlock, FULL));
        }
        // Ordinary keys and terminal queries do not allocate a receipt channel.
        let (completion, receiver) = if acknowledge {
            let (sender, receiver) = flume::bounded(1);
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        outstanding.bytes += bytes.len();
        outstanding.records += 1;
        let pending = Pending {
            bytes: bytes.to_vec(),
            shared: self.shared.clone(),
            completion,
            counted: true,
        };
        let result = self.sender.try_send(pending);
        drop(outstanding);
        match result {
            Ok(()) => Ok(WriteCompletion { receiver }),
            Err(flume::TrySendError::Full(pending)) => {
                drop(pending);
                Err(self.reject(io::ErrorKind::WouldBlock, FULL))
            }
            Err(flume::TrySendError::Disconnected(pending)) => {
                self.shared.fail("PTY input is closed".into());
                drop(pending);
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "PTY input is closed",
                ))
            }
        }
    }

    pub(crate) fn send(&self, bytes: &[u8]) -> Result<(), String> {
        self.enqueue(bytes, false)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn send_with_completion(&self, bytes: &[u8]) -> Result<WriteCompletion, String> {
        self.enqueue(bytes, true).map_err(|error| error.to_string())
    }
}

fn reject_queued(receiver: &flume::Receiver<Pending>, message: &str) {
    while let Ok(pending) = receiver.try_recv() {
        pending.complete(Err(message.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct GateWriter {
        entered: mpsc::Sender<Vec<u8>>,
        release: mpsc::Receiver<()>,
        written: mpsc::Sender<Vec<u8>>,
        dropped: mpsc::Sender<std::thread::ThreadId>,
    }

    impl Write for GateWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.entered.send(bytes.to_vec()).unwrap();
            self.release.recv().unwrap();
            self.written.send(bytes.to_vec()).unwrap();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for GateWriter {
        fn drop(&mut self) {
            let _ = self.dropped.send(std::thread::current().id());
        }
    }

    fn queued(
        writer: impl Write + Send + 'static,
        bytes: usize,
        records: usize,
    ) -> (QueuedWriter, InputErrors) {
        let (queued, errors, start) = QueuedWriter::prepare(writer, bytes, records);
        start();
        (queued, errors)
    }

    fn gate(
        bytes: usize,
        records: usize,
    ) -> (
        QueuedWriter,
        InputErrors,
        mpsc::Receiver<Vec<u8>>,
        mpsc::Sender<()>,
        mpsc::Receiver<Vec<u8>>,
        mpsc::Receiver<std::thread::ThreadId>,
    ) {
        let (entered, entering) = mpsc::channel();
        let (release, releasing) = mpsc::channel();
        let (written, writing) = mpsc::channel();
        let (dropped, dropping) = mpsc::channel();
        let (writer, errors) = queued(
            GateWriter {
                entered,
                release: releasing,
                written,
                dropped,
            },
            bytes,
            records,
        );
        (writer, errors, entering, release, writing, dropping)
    }

    #[test]
    fn stalled_sink_does_not_block_admission_and_inflight_bytes_count() {
        let (writer, errors, entering, release, writing, dropping) = gate(8, 8);
        writer.send(b"first").unwrap();
        assert_eq!(
            entering.recv_timeout(Duration::from_secs(2)).unwrap(),
            b"first"
        );
        let probe = writer.clone();
        let (admitted, admission) = mpsc::channel();
        std::thread::spawn(move || {
            admitted.send(probe.send(b"two")).unwrap();
        });
        admission
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(writer.send(b"x").unwrap_err().contains("not accepted"));
        assert!(errors.recv().unwrap().contains("not accepted"));
        assert!(writer.send(b"oversized").unwrap_err().contains("too large"));
        release.send(()).unwrap();
        assert_eq!(writing.recv().unwrap(), b"first");
        assert_eq!(entering.recv().unwrap(), b"two");
        release.send(()).unwrap();
        assert_eq!(writing.recv().unwrap(), b"two");
        drop(writer);
        assert_ne!(
            dropping.recv_timeout(Duration::from_secs(2)).unwrap(),
            std::thread::current().id()
        );
        assert!(entering.recv().is_err()); // No refused frame or partial admission.
    }

    #[test]
    fn inflight_record_counts_and_completions_follow_physical_fifo() {
        let (writer, _, entering, release, writing, dropping) = gate(100, 2);
        let first = writer.send_with_completion(b"key").unwrap();
        assert_eq!(
            entering.recv_timeout(Duration::from_secs(2)).unwrap(),
            b"key"
        );
        let second = writer.send_with_completion(b"query").unwrap();
        assert!(writer.send(b"third").is_err());
        let mut first_wait = std::pin::pin!(first.wait());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(first_wait.as_mut().poll(&mut context).is_pending());
        assert!(second.receiver.as_ref().unwrap().try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(writing.recv().unwrap(), b"key");
        assert_eq!(entering.recv().unwrap(), b"query");
        assert_eq!(
            first_wait.as_mut().poll(&mut context),
            std::task::Poll::Ready(Ok(())),
        );
        assert!(second.receiver.as_ref().unwrap().try_recv().is_err());
        // The completed record frees capacity while the next physical write stalls.
        writer.send(b"third").unwrap();
        release.send(()).unwrap();
        assert_eq!(writing.recv().unwrap(), b"query");
        assert_eq!(entering.recv().unwrap(), b"third");
        assert_eq!(second.receiver.unwrap().recv().unwrap(), Ok(()));
        release.send(()).unwrap();
        assert_eq!(writing.recv().unwrap(), b"third");
        drop(writer);
        dropping.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    struct PartialFailure {
        calls: mpsc::Sender<Vec<u8>>,
        release: mpsc::Receiver<()>,
        first: bool,
    }

    impl Write for PartialFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls.send(bytes.to_vec()).unwrap();
            if self.first {
                self.first = false;
                self.release.recv().unwrap();
                Ok(1)
            } else {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed sink"))
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn broken_pipe_is_visible_once_and_never_replays_partial_or_queued_frames() {
        let (calls, called) = mpsc::channel();
        let (release, releasing) = mpsc::channel();
        let (writer, errors) = queued(
            PartialFailure {
                calls,
                release: releasing,
                first: true,
            },
            100,
            2,
        );
        let first = writer.send_with_completion(b"abc").unwrap();
        assert_eq!(called.recv_timeout(Duration::from_secs(2)).unwrap(), b"abc");
        let second = writer.send_with_completion(b"queued").unwrap();
        // Leave an unread capacity warning pending. Permanent failure must
        // replace it rather than disappear behind the bounded error wakeup.
        assert!(writer.send(b"overflow").is_err());
        release.send(()).unwrap();
        assert_eq!(called.recv().unwrap(), b"bc");
        assert!(
            first
                .receiver
                .unwrap()
                .recv()
                .unwrap()
                .unwrap_err()
                .contains("closed sink")
        );
        assert!(
            second
                .receiver
                .unwrap()
                .recv()
                .unwrap()
                .unwrap_err()
                .contains("closed sink")
        );
        assert!(errors.recv().unwrap().contains("closed sink"));
        assert!(writer.send(b"retry").is_err());
        assert!(errors.try_recv().is_err());
        assert!(called.recv_timeout(Duration::from_secs(2)).is_err());
    }

    struct DropGate {
        entered: mpsc::Sender<std::thread::ThreadId>,
        release: mpsc::Receiver<()>,
    }

    impl Write for DropGate {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for DropGate {
        fn drop(&mut self) {
            self.entered.send(std::thread::current().id()).unwrap();
            self.release.recv().unwrap();
        }
    }

    #[test]
    fn dropping_the_queue_does_not_wait_for_a_blocking_physical_destructor() {
        let (entered, entering) = mpsc::channel();
        let (release, releasing) = mpsc::channel();
        let (writer, _) = queued(
            DropGate {
                entered,
                release: releasing,
            },
            10,
            10,
        );
        let (dropped, dropping) = mpsc::channel();
        std::thread::spawn(move || {
            drop(writer);
            dropped.send(std::thread::current().id()).unwrap();
        });
        let caller = dropping.recv_timeout(Duration::from_secs(2)).unwrap();
        let physical = entering.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_ne!(caller, physical);
        release.send(()).unwrap();
    }
}
