//! Ordered, non-blocking writes to a child-process agent.
//!
//! OMP and rpi RPC run as child processes. Their prompt, abort and extension
//! replies share one stdin pipe, so a single writer thread owns that pipe and
//! preserves command order. Sending only enqueues an owned command, so a large
//! prompt written to a slow-reading child can never block the UI thread.
//!
//! Backpressure bounds both command count and owned bytes. One cancellation slot
//! is reserved; adjacent pending aborts coalesce without crossing another command.
//! Broken pipes report one error and fail later sends immediately.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;
const MAX_QUEUED_COMMANDS: usize = 4_096;

enum WriteCommand {
    Prompt { id: String, text: String },
    Json(serde_json::Value),
    Shutdown,
}

struct Pending {
    command: WriteCommand,
    bytes: usize,
}

#[derive(Default)]
struct Admission {
    bytes: usize,
    commands: usize,
    last_abort: bool,
}

struct Shared {
    sender: mpsc::Sender<Pending>,
    admission: Mutex<Admission>,
    closed: AtomicBool,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Clone)]
pub(crate) struct AgentWriter {
    shared: Arc<Shared>,
}

impl AgentWriter {
    /// Start a writer thread that owns `sink`. `on_error` runs once on the
    /// writer thread if a write fails (including a broken pipe).
    pub(crate) fn spawn(
        mut sink: impl Write + Send + 'static,
        on_error: impl Fn(String) + Send + 'static,
    ) -> Self {
        let (sender, commands) = mpsc::channel::<Pending>();
        let shared = Arc::new(Shared {
            sender,
            admission: Mutex::new(Admission::default()),
            closed: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        let writer_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("agent-stdin".into())
            .spawn(move || {
                while let Ok(pending) = commands.recv() {
                    let value = match pending.command {
                        WriteCommand::Prompt { id, text } => serde_json::json!({
                            "type": "prompt", "message": text, "id": id,
                        }),
                        WriteCommand::Json(value) => value,
                        WriteCommand::Shutdown => break,
                    };
                    let result = match serde_json::to_writer(&mut sink, &value) {
                        Ok(()) => sink.write_all(b"\n").and_then(|()| sink.flush()),
                        Err(error) => Err(std::io::Error::other(error)),
                    };
                    {
                        let mut admission = writer_shared
                            .admission
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        admission.bytes -= pending.bytes;
                        admission.commands -= 1;
                        if admission.commands == 0 {
                            admission.last_abort = false;
                        }
                    }
                    if let Err(error) = result {
                        if !writer_shared.closed.swap(true, Ordering::AcqRel) {
                            on_error(format!("Failed to send agent command: {error}"));
                        }
                        break;
                    }
                }
            })
            .expect("agent writer thread");
        if let Ok(mut slot) = shared.thread.lock() {
            *slot = Some(thread);
        }
        Self { shared }
    }

    pub(crate) fn send_json(&self, command: serde_json::Value) -> Result<(), String> {
        let abort = command.get("type").and_then(|kind| kind.as_str()) == Some("abort");
        let bytes = json_owned_bytes(&command);
        self.enqueue(WriteCommand::Json(command), bytes, abort)
    }

    /// Queue an owned prompt without serializing it on the caller.
    pub(crate) fn send_prompt(&self, id: String, text: String) -> Result<(), String> {
        let bytes = id.len().saturating_add(text.len());
        self.enqueue(WriteCommand::Prompt { id, text }, bytes, false)
    }

    fn enqueue(&self, command: WriteCommand, bytes: usize, abort: bool) -> Result<(), String> {
        let mut admission = self
            .shared
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.is_closed() {
            return Err("The agent process has exited".into());
        }
        if abort && admission.last_abort {
            return Ok(());
        }
        // A reserved abort is a tiny fixed-shape record. It remains behind all
        // already admitted records, including a partially written prompt.
        if (!abort
            && (admission.commands >= MAX_QUEUED_COMMANDS
                || admission.bytes.saturating_add(bytes) > MAX_QUEUED_BYTES))
            || (abort && bytes > 1024)
        {
            return Err("The agent input queue is full; wait for the current command".into());
        }
        admission.bytes = admission.bytes.saturating_add(bytes);
        admission.commands += 1;
        admission.last_abort = abort;
        if self.shared.sender.send(Pending { command, bytes }).is_err() {
            admission.bytes -= bytes;
            admission.commands -= 1;
            self.close();
            return Err("The agent process has exited".into());
        }
        Ok(())
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Mark the writer closed (for example when the reader observed EOF).
    pub(crate) fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
    }

    /// Close the queue, wait for the writer to notice, and join it. Call after
    /// the child has been killed so a write blocked on backpressure returns.
    pub(crate) fn shutdown(&self) {
        self.close();
        let handle = self
            .shared
            .thread
            .lock()
            .ok()
            .and_then(|mut thread| thread.take());
        if let Some(handle) = handle {
            let _ = self.shared.sender.send(Pending {
                command: WriteCommand::Shutdown,
                bytes: 0,
            });
            let _ = handle.join();
        }
    }
}

/// Estimate owned JSON payload bytes without serializing or copying strings.
fn json_owned_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => text.len() + std::mem::size_of::<serde_json::Value>(),
        serde_json::Value::Array(values) => values.iter().map(json_owned_bytes).sum::<usize>() + 32,
        serde_json::Value::Object(values) => {
            values
                .iter()
                .map(|(key, value)| key.len() + json_owned_bytes(value))
                .sum::<usize>()
                + 32
        }
        _ => std::mem::size_of::<serde_json::Value>(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct SlowSink {
        bytes: Arc<Mutex<Vec<u8>>>,
        delay: Duration,
        fail: bool,
        release: Option<Arc<AtomicBool>>,
    }

    impl SlowSink {
        fn fast(bytes: Arc<Mutex<Vec<u8>>>, delay: Duration) -> Self {
            Self {
                bytes,
                delay,
                fail: false,
                release: None,
            }
        }
    }

    impl Write for SlowSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(release) = &self.release {
                while !release.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            thread::sleep(self.delay);
            if self.fail {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ));
            }
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn large_prompts_enqueue_without_waiting_for_a_slow_reader() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = AgentWriter::spawn(
            SlowSink::fast(Arc::clone(&bytes), Duration::from_millis(2)),
            |_| {},
        );

        let start = Instant::now();
        for index in 0..200 {
            writer
                .send_json(serde_json::json!({"type":"get_state","id":index}))
                .unwrap();
        }
        let prompt = "x".repeat(2 * 1024 * 1024);
        writer.send_prompt("kathamo-large".into(), prompt).unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "enqueue blocked for {:?}",
            start.elapsed()
        );
        writer.shutdown();

        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 201);
        assert!(lines[0].contains("\"id\":0"));
        assert!(lines[199].contains("\"id\":199"));
        assert!(lines[200].contains("\"type\":\"prompt\""));
    }

    #[test]
    fn a_broken_pipe_fails_once_and_then_rejects_sends() {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink_errors = Arc::clone(&errors);
        let writer = AgentWriter::spawn(
            SlowSink {
                bytes: Arc::new(Mutex::new(Vec::new())),
                delay: Duration::ZERO,
                fail: true,
                release: None,
            },
            move |error| sink_errors.lock().unwrap().push(error),
        );
        writer
            .send_json(serde_json::json!({"type":"abort"}))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while errors.lock().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(errors.lock().unwrap().len(), 1, "one reported failure");
        assert!(writer.is_closed());
        assert!(
            writer
                .send_json(serde_json::json!({"type":"abort"}))
                .is_err(),
            "a closed writer rejects new commands"
        );
        writer.shutdown();
    }

    #[test]
    fn prompt_queue_backpressure_is_reported_not_blocking() {
        let release = Arc::new(AtomicBool::new(false));
        let writer = AgentWriter::spawn(
            SlowSink {
                bytes: Arc::new(Mutex::new(Vec::new())),
                delay: Duration::ZERO,
                fail: false,
                release: Some(Arc::clone(&release)),
            },
            |_| {},
        );
        // The first prompt fills most of the queue and parks the writer; a
        // second one is rejected immediately instead of blocking or growing
        // without bound.
        writer
            .send_prompt("a".into(), "x".repeat(MAX_QUEUED_BYTES - 1))
            .unwrap();
        let start = Instant::now();
        assert!(writer.send_prompt("b".into(), "y".repeat(2)).is_err());
        assert!(start.elapsed() < Duration::from_millis(50));
        release.store(true, Ordering::Release);
        writer.shutdown();
    }

    #[test]
    fn all_command_payloads_are_bounded_and_cancellation_has_reserved_admission() {
        let release = Arc::new(AtomicBool::new(false));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = AgentWriter::spawn(
            SlowSink {
                bytes: Arc::clone(&bytes),
                delay: Duration::ZERO,
                fail: false,
                release: Some(Arc::clone(&release)),
            },
            |_| {},
        );
        assert!(
            writer
                .send_json(serde_json::json!({
                    "type":"extension_ui_response", "text":"x".repeat(MAX_QUEUED_BYTES + 1),
                }))
                .is_err()
        );
        for index in 0..MAX_QUEUED_COMMANDS {
            writer
                .send_prompt(index.to_string(), String::new())
                .unwrap();
        }
        assert!(
            writer
                .send_prompt("overflow".into(), String::new())
                .is_err()
        );
        writer
            .send_json(serde_json::json!({"type":"abort","id":"reserved"}))
            .unwrap();
        for _ in 0..1000 {
            writer
                .send_json(serde_json::json!({"type":"abort","id":"duplicate"}))
                .unwrap();
        }
        assert_eq!(
            writer.shared.admission.lock().unwrap().commands,
            MAX_QUEUED_COMMANDS + 1
        );
        release.store(true, Ordering::Release);
        writer.shutdown();
        let bytes = bytes.lock().unwrap();
        let records: Vec<serde_json::Value> = String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), MAX_QUEUED_COMMANDS + 1);
        assert_eq!(records.last().unwrap()["id"], "reserved");
    }
}
