//! Opt-in baseline: the pre-embedding rpi RPC subprocess bridge.
//!
//! The embedded bridge is the default. Setting `KATHAMO_RPI_BACKEND=rpc` keeps
//! the old process/stdio/JSONL path available for before/after measurements on
//! a credentialed machine, without changing the UI or the event contract.

use crate::agent_interaction::{self, Field, Kind, Request, Response};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use rpi_embedded::EventQueue;
use serde_json::{Value, json};

use super::{AgentEvent, coalesce_events, decode_message_update, decode_tool_event};
use crate::agent_writer::AgentWriter;

/// Pipe writes wait for startup on the stdin worker, never on the UI thread.
struct DeferredStdin {
    receiver: mpsc::Receiver<ChildStdin>,
    sink: Option<ChildStdin>,
}

#[derive(Default)]
struct Cleanup {
    done: Mutex<bool>,
    ready: std::sync::Condvar,
}
struct CleanupSignal(Arc<Cleanup>);
impl Drop for CleanupSignal {
    fn drop(&mut self) {
        *self.0.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.0.ready.notify_all();
    }
}

impl Write for DeferredStdin {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.sink.is_none() {
            self.sink =
                Some(self.receiver.recv().map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "rpi startup failed")
                })?);
        }
        self.sink.as_mut().expect("attached stdin").write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        match &mut self.sink {
            Some(sink) => sink.flush(),
            None => Ok(()),
        }
    }
}

/// UI handle only. The lifecycle worker exclusively owns spawn, kill, wait and
/// joins. Dropping this handle closes the output queue and requests teardown.
pub(crate) struct RpcAgentBridge {
    writer: AgentWriter,
    events: Arc<EventQueue<AgentEvent>>,
    requests: smol::channel::Receiver<Request>,
    stop: mpsc::Sender<()>,
    cleanup: Arc<Cleanup>,
    closed: bool,
    next_id: u64,
    trace_id: u64,
    cwd: PathBuf,
    binary: PathBuf,
    session_id: Arc<Mutex<Option<String>>>,
    model: Arc<Mutex<Option<String>>>,
}

impl RpcAgentBridge {
    pub(crate) fn spawn_resumed(
        cwd: &Path,
        session: Option<&str>,
        model: Option<&str>,
        trace_id: u64,
    ) -> Result<Self, String> {
        Self::spawn_session_with(Path::new("rpi"), cwd, session, model, trace_id, None)
    }

    pub(crate) fn poll_requests(&self) -> Vec<Request> {
        std::iter::from_fn(|| self.requests.try_recv().ok()).filter(Request::alive).collect()
    }

    pub(crate) fn session_id(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|id| id.clone())
    }

    fn spawn_session_with(
        binary: &Path,
        cwd: &Path,
        session: Option<&str>,
        model: Option<&str>,
        trace_id: u64,
        after_cleanup: Option<Arc<Cleanup>>,
    ) -> Result<Self, String> {
        let events = Arc::new(EventQueue::default());
        let (request_tx, requests) = smol::channel::bounded(16);
        let error_events = Arc::clone(&events);
        let (stdin_tx, stdin_rx) = mpsc::channel();
        let writer = AgentWriter::spawn(
            DeferredStdin {
                receiver: stdin_rx,
                sink: None,
            },
            move |error| {
                emit(&error_events, AgentEvent::Error(error));
            },
        );
        writer.send_json(json!({"type":"get_state","id":"kathamo-session"}))?;
        let session_id = Arc::new(Mutex::new(session.map(str::to_owned)));
        let model_id = Arc::new(Mutex::new(model.map(str::to_owned)));
        let (stop, stopped) = mpsc::channel();
        let cleanup = Arc::new(Cleanup::default());
        let cleaned = Arc::clone(&cleanup);
        let mut command = Command::new(binary);
        command
            .args(["--mode", "rpc"])
            .arg("--session-dir")
            .arg(crate::paths::rpi_sessions_dir())
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(session) = session {
            command.args(["--session", session]);
        }
        if let Some(model) = model {
            command.args(["--model", model]);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let worker_events = Arc::clone(&events);
        let worker_writer = writer.clone();
        let worker_session = Arc::clone(&session_id);
        let worker_model = Arc::clone(&model_id);
        let reader_stop = stop.clone();
        let worker_cwd = cwd.to_path_buf();
        if let Err(error) = thread::Builder::new().name("rpi-rpc-lifecycle".into()).spawn(move || {
            let _cleaned = CleanupSignal(cleaned);
            if let Some(predecessor) = after_cleanup {
                let mut done = predecessor.done.lock().unwrap_or_else(|e| e.into_inner());
                while !*done {
                    done = predecessor.ready.wait(done).unwrap_or_else(|e| e.into_inner());
                }
            }
            if stopped.try_recv().is_ok() {
                worker_writer.close();
                drop(stdin_tx);
                worker_writer.shutdown();
                return;
            }
            let start = std::time::Instant::now();
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    // Disconnect deferred stdin before joining its writer.
                    worker_writer.close();
                    drop(stdin_tx);
                    emit(&worker_events, AgentEvent::Error(format!(
                        "Could not start rpi in {}: {error}. Install rpi and ensure it is on PATH.",
                        worker_cwd.display()
                    )));
                    emit(&worker_events, AgentEvent::Done);
                    emit(&worker_events, AgentEvent::Closed);
                    worker_writer.shutdown();
                    return;
                }
            };
            crate::stream_timing::record_since("engine.rpc.spawn", start);
            let _ = stdin_tx.send(child.stdin.take().expect("piped stdin"));
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr_pipe = child.stderr.take().expect("piped stderr");
            let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
            let stderr_sink = Arc::clone(&stderr);
            let stderr_thread = thread::spawn(move || {
                let mut reader = BufReader::new(stderr_pipe);
                // Bound diagnostics even if the child never writes a newline.
                let mut bytes = [0u8; 4096];
                use std::io::Read;
                while let Ok(count) = reader.read(&mut bytes) {
                    if count == 0 { break; }
                    let mut sink = stderr_sink.lock().unwrap_or_else(|e| e.into_inner());
                    sink.extend_from_slice(&bytes[..count]);
                    if sink.len() > 16_384 {
                        let mut start = sink.len() - 16_384;
                        while start < sink.len() && sink[start] & 0xc0 == 0x80 { start += 1; }
                        sink.drain(..start);
                    }
                }
            });
            let reader_events = Arc::clone(&worker_events);
            let reply_writer = worker_writer.clone();
            let reader = thread::spawn(move || {
                let mut active_dialogs = std::collections::HashMap::new();
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                let started = std::time::Instant::now();
                let mut first_record = true;
                let failure = loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break None,
                        Err(error) => break Some(format!("Failed reading rpi output: {error}")),
                        Ok(_) => {}
                    }
                    let parse_start = std::time::Instant::now();
                    let record: Value = match serde_json::from_str(&line) {
                        Ok(record) => record,
                        Err(error) => {
                            if !emit(&reader_events, AgentEvent::Error(format!("Invalid rpi RPC output: {error}"))) {
                                break None;
                            }
                            continue;
                        }
                    };
                    crate::stream_timing::record_since("bridge.rpc.decode", parse_start);
                    if first_record {
                        first_record = false;
                        crate::stream_timing::record_since("engine.rpc.first_record", started);
                    }
                    if record["type"] == "response" && record["command"] == "get_state" {
                        if let Some(id) = record["data"]["sessionId"].as_str() {
                            *worker_session.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_owned());
                        }
                        if let (Some(provider), Some(model)) = (
                            record["data"]["provider"].as_str(), record["data"]["modelId"].as_str(),
                        ) {
                            *worker_model.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{provider}/{model}"));
                        }
                    }
                    if record["type"] == "ui_dialog_request" {
                        if let Some((id, close)) = handle_native_dialog(&record, &reply_writer, &request_tx) {
                            active_dialogs.insert(id, close);
                        }
                    } else if record["type"] == "ui_dialog_closed" {
                        if let Some(id) = record["requestId"].as_str()
                            && let Some(close) = active_dialogs.remove(id)
                        {
                            close.close();
                            super::signal_wake();
                        }
                    } else if record["type"] == "extension_ui_request" {
                        handle_extension_ui(&record, &reply_writer, &reader_events, &request_tx);
                    } else {
                        let project_start = std::time::Instant::now();
                        let decoded = decode_event(&record);
                        crate::stream_timing::record_since("bridge.rpc.project", project_start);
                        for event in decoded {
                            match &event {
                                AgentEvent::Text(text) if !text.is_empty() =>
                                    crate::stream_timing::turn_delta_arrived(trace_id, crate::stream_timing::DeltaKind::Answer),
                                AgentEvent::Thinking(text) if !text.is_empty() =>
                                    crate::stream_timing::turn_delta_arrived(trace_id, crate::stream_timing::DeltaKind::Reasoning),
                                AgentEvent::ToolEnd { id: Some(id), .. } =>
                                    crate::stream_timing::turn_tool_finished(trace_id, id),
                                _ => {}
                            }
                            if !emit(&reader_events, event) { break; }
                        }
                    }
                };
                for (_, pending) in active_dialogs {
                    pending.close();
                }
                reply_writer.close();
                let _ = reader_stop.send(());
                failure
            });
            // EOF and handle drop both wake the lifecycle owner. No UI joins.
            let _ = stopped.recv();
            terminate(&mut child);
            worker_writer.shutdown();
            let failure = reader.join().ok().flatten();
            let _ = stderr_thread.join();
            let diagnostic = String::from_utf8_lossy(&stderr.lock().unwrap_or_else(|e| e.into_inner())).trim().to_owned();
            let mut error = failure.unwrap_or_else(|| "rpi process exited".into());
            if !diagnostic.is_empty() { error.push_str(&format!(": {diagnostic}")); }
            emit(&worker_events, AgentEvent::Error(error));
            emit(&worker_events, AgentEvent::Done);
            emit(&worker_events, AgentEvent::Closed);
        }) {
            events.close();
            writer.close();
            return Err(format!("Could not start rpi lifecycle worker: {error}"));
        }
        Ok(Self {
            writer,
            events,
            requests,
            stop,
            closed: false,
            next_id: 0,
            trace_id,
            cleanup,
            cwd: cwd.to_path_buf(),
            binary: binary.to_path_buf(),
            session_id,
            model: model_id,
        })
    }

    pub(crate) fn restart_after_auth(&mut self) -> Result<(), String> {
        let session = self
            .session_id()
            .ok_or("The agent session is still starting; try again shortly")?;
        let model = self
            .model
            .lock()
            .map_err(|_| "rpi model lock was poisoned")?
            .clone()
            .ok_or("The agent model is still starting; try again shortly")?;
        let predecessor = Some(Arc::clone(&self.cleanup));
        let replacement = Self::spawn_session_with(
            &self.binary,
            &self.cwd,
            Some(&session),
            Some(&model),
            self.trace_id,
            predecessor,
        )?;
        *self = replacement;
        Ok(())
    }

    pub(crate) fn send_prompt(&mut self, prompt: &str) -> Result<(), String> {
        if self.closed {
            return Err("The rpi process has exited".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.writer
            .send_prompt(format!("kathamo-{}", self.next_id), prompt.to_owned())
    }

    pub(crate) fn steer(&self, text: &str) -> Result<(), String> {
        self.writer.send_json(json!({"type":"queue_message","deliverAs":"steer","message":text}))
    }

    pub(crate) fn cancel(&mut self) -> Result<(), String> {
        if self.closed {
            return Err("The rpi process has exited".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.writer
            .send_json(json!({"type":"abort","id":format!("kathamo-{}", self.next_id)}))
    }

    pub(crate) fn poll(&mut self) -> Vec<AgentEvent> {
        let (events, more) = self.events.drain();
        if events
            .iter()
            .any(|event| matches!(event, AgentEvent::Closed))
        {
            self.closed = true;
        }
        if more {
            super::signal_wake();
        }
        coalesce_events(events)
    }
}

impl Drop for RpcAgentBridge {
    fn drop(&mut self) {
        self.events.close();
        self.writer.close();
        let _ = self.stop.send(());
    }
}

fn terminate(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn emit(events: &EventQueue<AgentEvent>, event: AgentEvent) -> bool {
    let sent = events.send_blocking(event).is_ok();
    if sent {
        super::signal_wake();
    }
    sent
}

fn handle_native_dialog(
    record: &Value,
    writer: &AgentWriter,
    requests: &smol::channel::Sender<Request>,
) -> Option<(String, smol::channel::Sender<Response>)> {
    let id = record["requestId"].as_str()?.to_owned();
    let ui = &record["ui"];
    let question = ui["questions"].as_array().and_then(|questions| questions.first()).unwrap_or(ui);
    let prompt = question["question"].as_str()
        .or_else(|| question["title"].as_str()).unwrap_or("Extension request");
    let options: Vec<String> = question["options"].as_array().or_else(|| ui["options"].as_array())
        .into_iter().flatten().filter_map(|option| option.as_str().or_else(|| option["title"].as_str()))
        .map(str::to_owned).collect();
    let choices = !options.is_empty();
    let permission = ui["purpose"] == "permission";
    let confirm = !choices && ui["kind"] == "confirm";
    let custom = !permission && (!choices || question["allowFreeform"] == true
        || question["allow_custom"] == true || ui["allowFreeform"] == true);
    let fields = if confirm {
        Vec::new()
    } else {
        vec![Field {
            label: prompt.to_owned(),
            kind: if !choices { Kind::Text }
                else if !permission && (question["allowMultiple"] == true || question["multiple"] == true || ui["allowMultiple"] == true) { Kind::Multiple }
                else { Kind::Single },
            options: options.clone(),
            required: true,
            custom,
        }]
    };
    let title = if confirm { prompt } else if permission { "Permission request" } else { "Extension question" };
    let (mut request, reply) = agent_interaction::request(
        title.into(), question["context"].as_str().or_else(|| ui["message"].as_str()).map(str::to_owned),
        fields, confirm,
    );
    request.allow_session = false;
    let close = request.reply.clone();
    if requests.try_send(request).is_err() {
        let _ = writer.send_json(json!({"type":"ui_dialog_response","requestId":id,"cancelled":true}));
        return None;
    }
    super::signal_wake();
    let writer = writer.clone();
    let reply_id = id.clone();
    std::thread::spawn(move || {
        let message = match reply.recv_blocking() {
            Ok(Response::Submit(values)) => {
                let value = &values[0]["value"];
                let chosen = value.as_array().is_some()
                    || value.as_str().is_some_and(|text| options.iter().any(|option| option == text));
                let answer = if chosen {
                    let values = value.as_array().cloned().unwrap_or_else(|| vec![value.clone()]);
                    json!({"kind":"selector","values":values})
                } else {
                    json!({"kind":"input","value":value,"text":value,"values":[value]})
                };
                json!({"type":"ui_dialog_response","requestId":reply_id,"answer":answer})
            }
            Ok(Response::AllowOnce) => json!({"type":"ui_dialog_response","requestId":reply_id,"answer":true}),
            Ok(Response::Deny) => json!({"type":"ui_dialog_response","requestId":reply_id,"cancelled":true}),
            _ => return,
        };
        let _ = writer.send_json(message);
    });
    Some((id, close))
}

fn handle_extension_ui(
    record: &Value,
    writer: &AgentWriter,
    events: &EventQueue<AgentEvent>,
    requests: &smol::channel::Sender<Request>,
) {
    let method = record["method"].as_str().unwrap_or("");
    match method {
        "select" | "confirm" | "input" | "editor" => {
            let fields = if method == "confirm" {
                Vec::new()
            } else {
                vec![Field {
                    label: if method == "select" { "Choose an option" } else { "Your response" }.into(),
                    kind: if method == "select" { Kind::Single } else { Kind::Text },
                    options: record["options"].as_array().into_iter().flatten()
                        .filter_map(Value::as_str).map(str::to_owned).collect(),
                    required: method == "select",
                    custom: false,
                }]
            };
            let (mut request, receiver) = agent_interaction::request(
                record["title"].as_str().unwrap_or("Extension request").into(),
                record["message"].as_str().map(str::to_owned),
                fields, method == "confirm",
            );
            request.allow_session = false;
            let id = record["id"].clone();
            if requests.try_send(request).is_err() {
                let _ = writer.send_json(json!({"type":"extension_ui_response","id":id,"cancelled":true}));
                return;
            }
            super::signal_wake();
            let writer = writer.clone();
            let confirm = method == "confirm";
            std::thread::spawn(move || {
                let mut result = json!({"type":"extension_ui_response","id":id});
                match receiver.recv_blocking() {
                    Ok(Response::Submit(values)) => result["value"] = values[0]["value"].clone(),
                    Ok(Response::AllowOnce | Response::AllowSession) => result["confirmed"] = json!(true),
                    _ if confirm => result["confirmed"] = json!(false),
                    _ => result["cancelled"] = json!(true),
                }
                let _ = writer.send_json(result);
            });
        }
        "notify" => {
            if let Some(message) = record["message"].as_str() {
                emit(
                    events,
                    if record["notifyType"] == "error" {
                        AgentEvent::Error(message.to_owned())
                    } else {
                        AgentEvent::Tool(message.to_owned())
                    },
                );
            }
        }
        _ => {}
    }
}

fn decode_event(record: &Value) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    match record["type"].as_str().unwrap_or("") {
        "response" => {
            let command = record["command"].as_str().unwrap_or("command");
            if record["success"] == false {
                let error = record["error"].as_str().unwrap_or("unknown error");
                events.push(AgentEvent::Error(format!("rpi {command}: {error}")));
                if command == "prompt" {
                    events.push(AgentEvent::Done);
                }
            } else if command == "prompt" && record["data"]["disposition"] == "handled" {
                events.push(AgentEvent::Done);
            }
        }
        "message_update" => {
            if let Some(event) = decode_message_update(record) {
                events.push(event);
            }
        }
        "message_end" => {
            let message = &record["message"];
            if message["role"] == "assistant"
                && let Some(usage) = message["usage"].as_object()
            {
                let count = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
                let cost = usage
                    .get("cost")
                    .and_then(|cost| cost["total"].as_f64())
                    .filter(|cost| cost.is_finite() && *cost > 0.0);
                events.push(AgentEvent::Usage(rpi_embedded::AgentUsage {
                    input: count("input"),
                    output: count("output"),
                    cache_read: count("cacheRead"),
                    cache_write: count("cacheWrite"),
                    cost,
                }));
            }
            if message["role"] == "assistant" && message["stopReason"] == "error" {
                let error = message["errorMessage"]
                    .as_str()
                    .unwrap_or("The agent encountered an error");
                events.push(AgentEvent::Error(error.to_owned()));
            }
        }
        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
            if let Some(event) = decode_tool_event(record) {
                events.push(event);
            }
        }
        "agent_settled" => events.push(AgentEvent::Done),
        "extension_error" => {
            let error = record["error"]
                .as_str()
                .unwrap_or("Unknown extension error");
            events.push(AgentEvent::Error(format!("rpi extension: {error}")));
        }
        _ => {}
    }
    events
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn usage_finalized_rpc_message_includes_failed_request_tokens() {
        let events = decode_event(&json!({
            "type": "message_end",
            "message": {
                "role": "assistant", "stopReason": "error", "errorMessage": "interrupted",
                "usage": {
                    "input": 10, "output": 3, "cacheRead": 20, "cacheWrite": 5,
                    "cost": {"total": 0.001}
                }
            }
        }));
        assert!(matches!(&events[..], [
            AgentEvent::Usage(usage), AgentEvent::Error(error)
        ] if usage.input == 10 && usage.output == 3 && usage.cache_read == 20
            && usage.cache_write == 5 && usage.cost == Some(0.001) && error == "interrupted"));
        let events = decode_event(&json!({
            "type": "message_end",
            "message": {"role": "assistant", "usage": {"input": 2, "cost": {"total": 0.0}}}
        }));
        assert!(matches!(&events[..], [AgentEvent::Usage(usage)] if usage.cost.is_none()));
    }
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    #[test]
    fn a_slow_rpc_reader_does_not_block_prompt_abort_or_drop() {
        let dir = std::env::temp_dir().join(crate::paths::uid("kathamo-slow-rpc"));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("slow-rpi.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho $$ > pid\nsleep 30 &\necho $! > descendant\nwait\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut bridge =
            RpcAgentBridge::spawn_session_with(&script, &dir, None, None, 1, None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while std::fs::read_to_string(dir.join("descendant"))
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok())
            .is_none()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        let pid: i32 = std::fs::read_to_string(dir.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let descendant: i32 = std::fs::read_to_string(dir.join("descendant"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let prompt = "x".repeat(4 * 1024 * 1024);
        let start = Instant::now();
        bridge.send_prompt(&prompt).unwrap();
        let prompt_time = start.elapsed();
        let start = Instant::now();
        bridge.cancel().unwrap();
        let abort_time = start.elapsed();
        assert!(
            prompt_time < Duration::from_millis(100),
            "prompt enqueue took {prompt_time:?}"
        );
        assert!(
            abort_time < Duration::from_millis(50),
            "abort enqueue took {abort_time:?}"
        );
        let start = Instant::now();
        drop(bridge);
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "RPC drop blocked"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "lifecycle worker must reap its child"
        );
        let descendant_dead = || {
            #[cfg(target_os = "linux")]
            {
                std::fs::read_to_string(format!("/proc/{descendant}/stat"))
                    .map(|stat| {
                        stat.rsplit_once(')')
                            .is_some_and(|(_, fields)| fields.trim_start().starts_with('Z'))
                    })
                    .unwrap_or(true)
            }
            #[cfg(not(target_os = "linux"))]
            {
                (unsafe { libc::kill(descendant, 0) }) == -1
            }
        };
        while !descendant_dead() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            descendant_dead(),
            "RPC descendants must be terminated with their process group"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asynchronous_start_failure_delivers_error_done_closed_in_order() {
        let mut bridge = RpcAgentBridge::spawn_session_with(
            Path::new("/definitely-missing-rpi-binary"),
            &std::env::temp_dir(),
            None,
            None,
            2,
            None,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            events.extend(bridge.poll());
            if events
                .iter()
                .any(|event| matches!(event, AgentEvent::Closed))
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            &events[..],
            [AgentEvent::Error(_), AgentEvent::Done, AgentEvent::Closed]
        ));
    }
}
