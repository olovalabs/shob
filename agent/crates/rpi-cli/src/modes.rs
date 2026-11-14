//! Output modes. Mirrors the v1-relevant slice of the TS
//! `packages/coding-agent/src/modes/{print-mode,json-event,rpc-mode}.ts` — the
//! three run shapes a harness-backed CLI needs:
//!
//! - [`print`] — single-shot: send the prompt(s), print the final assistant
//!   text (or the error) to stdout, exit. Mirrors TS `runPrintMode` (text mode).
//! - [`json`] — single-shot streaming: emit each harness event as a JSON line
//!   on stdout, then the final outcome. Mirrors TS `runPrintMode`
//!   (`mode === "json"`) + [`json_event::toJsonEvent`].
//! - [`interactive`] — a minimal line-oriented REPL: read prompts from stdin,
//!   run each, print the assistant text, loop until EOF / `/exit`. v1 does NOT
//!   port the TS `InteractiveMode` TUI (`modes/interactive/*` — a full terminal
//!   UI with Ink/React components); this is a deliberately minimal replacement,
//!   documented in the initial port notes (retired).
//!
//! All three drive the same `AgentHarness` via `AgentLane::prompt_text`.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex};

use rpi_agent::events::AgentEvent;
use rpi_ai::types::{AssistantMessage, Content, ImageContent, StopReason, UserMessage};
use rpi_harness::agent_harness::{
    AgentHarness, AgentLane, CompactionOutcome, CompactionResult, HarnessRunOutcome,
};
use rpi_harness::events::{HarnessEvent, RunEndOutcome};
use rpi_harness::session::session::SessionEntryCommit;
use rpi_harness::session::types::{
    BranchBounds, Entry, EntryCursor, EntryOrder, EntryQuery, LaneRecord, RecordQuery,
};

use crate::args::Args;
use crate::rpc::protocol as rpc_protocol;
use crate::rpc::writer::{start_rpc_writer, write_agent_event, write_line, RpcWriter};

/// Extract the concatenated text content from an assistant message. Mirrors the
/// TS print-mode loop (`for content of assistantMsg.content if type===text`).
pub fn assistant_text(msg: &AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

/// The exit code a run's outcome maps to. Mirrors TS print mode: error/aborted
/// ⇒ exit 1; everything else ⇒ 0.
pub fn outcome_exit_code(outcome: &HarnessRunOutcome) -> i32 {
    match outcome {
        HarnessRunOutcome::Failed { .. } | HarnessRunOutcome::Aborted { .. } => 1,
        _ => 0,
    }
}

/// `print` mode: send the initial message (prompt text + inline `@file`
/// expansions), then any follow-up messages, print the final assistant text,
/// return the exit code. Mirrors TS `runPrintMode` (text).
pub async fn print(
    harness: &AgentHarness,
    args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");

    let mut last_exit = 0;
    let mut persistence_failed = false;
    let mut last_msg: Option<AssistantMessage> = None;

    // The initial prompt (and its `@file` attachments) go in one user message;
    // extra positionals are separate prompts (mirrors the TS loop).
    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }

    if prompts.is_empty() {
        // Nothing to do — print mode with no prompt is a no-op success.
        return 0;
    }

    let mut images = initial_images;
    for prompt in prompts {
        let outcome = lane.prompt_text(&prompt, std::mem::take(&mut images)).await;
        // Print mode has no transcript to report into, so a write failure goes to
        // stderr. Silence here is what let a conversation stop being saved while
        // the run looked fine.
        for failure in lane.take_write_failures() {
            persistence_failed |= args.experimental_native_store.is_some();
            eprintln!(
                "session not saved: could not persist {} ({})",
                failure.what, failure.message
            );
        }
        match outcome {
            Ok(result) => {
                last_exit = outcome_exit_code(&result.outcome);
                match &result.outcome {
                    HarnessRunOutcome::Completed { final_message, .. }
                    | HarnessRunOutcome::Aborted { final_message, .. } => {
                        last_msg = Some(final_message.clone());
                    }
                    HarnessRunOutcome::Failed {
                        error,
                        final_message,
                        ..
                    } => {
                        if let Some(m) = final_message {
                            if m.stop_reason == StopReason::Error {
                                if let Some(em) = &m.error_message {
                                    eprintln!("{em}");
                                }
                            }
                        }
                        if error.code == "SessionStorage" {
                            eprintln!("{}", session_storage_notice(&error.message));
                        } else {
                            eprintln!("run failed: {error:?}");
                        }
                    }
                    HarnessRunOutcome::Suspended { .. } => {
                        eprintln!("run suspended (deferred) — resume is not supported in v1");
                        last_exit = 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("prompt rejected: {e}");
                return 1;
            }
        }
    }

    // Print the final assistant text to stdout (TS: writeRawStdout text + "\n").
    if let Some(m) = &last_msg {
        match m.stop_reason {
            StopReason::Error => {
                if let Some(em) = &m.error_message {
                    eprintln!("{em}");
                }
                last_exit = 1;
            }
            StopReason::Aborted => {
                eprintln!("request aborted");
                last_exit = 1;
            }
            _ => {
                let text = assistant_text(m);
                let mut out = std::io::stdout();
                let _ = out.write_all(text.as_bytes());
                if !text.ends_with('\n') {
                    let _ = out.write_all(b"\n");
                }
                let _ = out.flush();
            }
        }
    }

    if persistence_failed {
        1
    } else {
        last_exit
    }
}

/// `json` mode: emit each harness event as a JSON line on stdout, run the
/// prompts, then emit a terminal `result` line carrying the outcome + final
/// text. Mirrors TS `runPrintMode` (`mode === "json"`) streaming every event.
pub async fn json(
    harness: &AgentHarness,
    args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
    mut agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let collected: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_watch = collected.clone();

    // A watch captures every event (RunStart fires inline during prompt_text,
    // before a post-call listener could attach — same reason as the M5g test).
    let mut watch = harness.events().watch(|| ());
    watch.start(Arc::new(move |event: &HarnessEvent| {
        // Emit each event live as JSON, and also buffer for the final summary.
        emit_json_event(event);
        collected_for_watch.lock().unwrap().push(event.clone());
    }));
    // Keep the watch alive for the whole run. Leaking is acceptable for a
    // single-shot CLI process (the bus outlives this scope anyway).
    std::mem::forget(watch);

    // The harness bus carries run lifecycle events; the agent receiver carries
    // the native fine-grained stream (turns, message deltas, and tools).
    let (agent_done_tx, mut agent_done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let agent_event_task = agent_events.take().map(|mut rx| {
        tokio::spawn(async move {
            let done_tx = agent_done_tx;
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let terminal = event.is_terminal();
                        emit_agent_event(&event);
                        if terminal {
                            let _ = done_tx.send(());
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    });

    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }

    let mut last_exit = 0;
    let mut persistence_failed = false;
    let mut final_outcome: Option<HarnessRunOutcome> = None;

    let mut images = initial_images;
    for prompt in prompts {
        match lane.prompt_text(&prompt, std::mem::take(&mut images)).await {
            Ok(result) => {
                // The harness resolves after its run outcome, while the
                // broadcast listener may still be scheduling the terminal
                // AgentEnd line. Wait briefly so JSON consumers see the full
                // lifecycle before the final result summary. The timeout is
                // deliberately bounded for custom/older harness emitters.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    agent_done_rx.recv(),
                )
                .await;
                last_exit = outcome_exit_code(&result.outcome);
                final_outcome = Some(result.outcome);
            }
            Err(e) => {
                // Emit a structured error line + exit.
                let line = serde_json::json!({
                    "type": "error",
                    "error": e.to_string(),
                });
                println!("{line}");
                if args.experimental_native_store.is_some() {
                    for failure in lane.take_write_failures() {
                        persistence_failed = true;
                        println!(
                            "{}",
                            serde_json::json!({"type":"error","kind":"session_write_failed","error":failure.message,"what":failure.what})
                        );
                    }
                    last_exit = 1;
                    break;
                }
                if let Some(task) = agent_event_task {
                    task.abort();
                }
                return 1;
            }
        }
        if args.experimental_native_store.is_some() {
            for failure in lane.take_write_failures() {
                persistence_failed = true;
                println!(
                    "{}",
                    serde_json::json!({"type":"error","kind":"session_write_failed","error":failure.message,"what":failure.what})
                );
            }
        }
    }

    // Terminal result summary.
    let (outcome_str, final_text) = match final_outcome {
        Some(HarnessRunOutcome::Completed { final_message, .. }) => {
            ("completed", Some(assistant_text(&final_message)))
        }
        Some(HarnessRunOutcome::Aborted { final_message, .. }) => {
            ("aborted", Some(assistant_text(&final_message)))
        }
        Some(HarnessRunOutcome::Failed { final_message, .. }) => {
            let t = final_message.as_ref().map(assistant_text);
            ("failed", t)
        }
        Some(HarnessRunOutcome::Suspended { .. }) => ("suspended", None),
        None if last_exit != 0 => ("failed", None),
        None => ("idle", None),
    };
    let mut result_line = serde_json::json!({
        "type": "result",
        "outcome": if persistence_failed{"failed"}else{outcome_str},
        "finalText": final_text,
    });
    if persistence_failed {
        result_line["persistenceFailed"] = true.into();
        last_exit = 1;
    }
    println!("{result_line}");
    if let Some(task) = agent_event_task {
        task.abort();
    }
    last_exit
}

fn emit_agent_event(event: &AgentEvent) {
    println!("{}", agent_event_json(event));
}

/// `rpc` mode: a headless JSONL command loop (the server-side agent).
///
/// Reads one JSON command per line from stdin (`{id?, type, ...}`), drives the
/// `main` lane, and writes one JSON object per line to stdout:
/// - fine-grained `AgentEvent`s via [`agent_event_json`] (each carries `type`),
/// - a `{"type":"response","id":<id>,"status":"ok"|"error",...}` per command.
///
/// Commands: `prompt`, `resume_pending`, and `compact` (drive the lane),
/// `abort`, `get_state`, `set_model`,
/// `set_thinking_level`, `set_active_tools`, `ping`, `stop`. `stop` (or stdin
/// EOF) ends the loop. This is the process `rpi-server` spawns and that
/// `rpi --connect` talks to. Mirrors TS `runRpcMode` / `modes/rpc`.
///
/// The extension `UiDialog` mailbox is bridged over the same channel: the host
/// emits `{"type":"ui_dialog_request","requestId","toolCallId","ui"}`, and the
/// client answers with `{"type":"ui_dialog_response","requestId","answer"}` (or
/// `"cancelled":true`). A `prompt` runs in its own task so this loop keeps
/// reading — that is what lets `abort` and dialog answers arrive mid-run.
pub async fn rpc(
    harness: &AgentHarness,
    mut agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
    ui_dialog: rpi_extensions::UiDialogMailbox,
    oauth_snapshot: Option<Arc<rpi_extensions::RegistrySnapshot>>,
    message_updates: crate::args::RpcMessageUpdates,
) -> i32 {
    rpc_with_io(
        harness,
        agent_events.take(),
        model_catalog,
        ui_dialog,
        oauth_snapshot,
        message_updates,
        tokio::io::BufReader::new(tokio::io::stdin()),
        std::io::stdout(),
    )
    .await
}

/// Opt-in, payload-free stage markers. An unmatched begin identifies an
/// unfinished await, not its cause. Enqueue completion is NOT pipe delivery.
struct RpcPhaseTiming {
    phase: &'static str,
    started: Option<std::time::Instant>,
}

impl RpcPhaseTiming {
    fn start(enabled: bool, phase: &'static str) -> Self {
        let started = enabled.then(std::time::Instant::now);
        if enabled {
            eprintln!("RPI_RPC_DIAGNOSTICS phase={phase} event=begin");
        }
        Self { phase, started }
    }

    fn finish(self) {
        if let Some(started) = self.started {
            eprintln!(
                "RPI_RPC_DIAGNOSTICS phase={} event=end elapsed_ns={}",
                self.phase,
                started.elapsed().as_nanos()
            );
        }
    }
}

async fn rpc_with_io<R, W>(
    harness: &AgentHarness,
    mut agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
    ui_dialog: rpi_extensions::UiDialogMailbox,
    oauth_snapshot: Option<Arc<rpi_extensions::RegistrySnapshot>>,
    message_updates: crate::args::RpcMessageUpdates,
    mut input: R,
    output: W,
) -> i32
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: Write + Send + 'static,
{
    let phase_diagnostics =
        std::env::var_os("RPI_RPC_DIAGNOSTICS").is_some_and(|value| value != "0");
    let setup_timing = RpcPhaseTiming::start(phase_diagnostics, "setup_to_ready_enqueue");
    let mut command_tasks: Vec<RetainedRpcTask<()>> = Vec::new();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    // One blocking writer owns stdout. Producers only enqueue bounded messages,
    // so pipe backpressure never blocks a Tokio worker on IO or a std mutex.
    let (stdout, writer_task, writer_failed) = start_rpc_writer(output, message_updates);
    let writer_task = RetainedRpcTask::new(writer_task, writer_failed.clone());
    let pump_stop = tokio_util::sync::CancellationToken::new();
    let external_tools =
        crate::rpc::external_tools::ExternalTools::new(stdout.clone(), Arc::downgrade(&lane));
    let mut external_names = Vec::new();
    let file_io_router = crate::session::file_io_router(harness);
    let mut owner = RpcOwner {
        harness,
        file_io: None,
        backend: external_tools.clone(),
        run_cancel: tokio_util::sync::CancellationToken::new(),
        pump_stop: pump_stop.clone(),
        failed: writer_failed.clone(),
        mailbox: ui_dialog.clone(),
        armed: true,
    };

    // Publish generic commit metadata after successful Session appends. The
    // event is intentionally not a durable-state payload: consumers reread
    // the canonical branch and use this only as an invalidation signal.
    let (commit_tx, mut commit_rx) = tokio::sync::broadcast::channel::<SessionEntryCommit>(256);
    harness
        .session()
        .set_entry_commit_publisher(Some(commit_tx.clone()));
    let commit_task = {
        let out = stdout.clone();
        tokio::spawn(async move {
            while let Some(event) =
                crate::rpc::tool_program_queries::commit_stream_event(commit_rx.recv().await)
            {
                write_line(&out, &event).await;
            }
        })
    };

    let commit_task = RetainedRpcTask::new(commit_task, pump_stop.clone());

    // A completion response is queued only after its final events, so a
    // subsequent prompt cannot overtake the previous run's agent_end.
    let (event_flush_tx, mut event_flush_rx) =
        tokio::sync::mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<()>>();
    // Stream fine-grained AgentEvents as JSONL until the receiver closes.
    let event_task = agent_events.take().map(|mut rx| {
        let out = stdout.clone();
        let stop = pump_stop.clone();
        tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    _ = stop.cancelled() => {
                        drain_rpc_events(&out, &mut rx).await;
                        break;
                    }
                    Some(done) = event_flush_rx.recv() => {
                        drain_rpc_events(&out, &mut rx).await;
                        let _ = done.send(());
                        continue;
                    }
                    event = rx.recv() => event,
                };
                match received {
                    Ok(event) => write_agent_event(&out, &event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    });

    let event_task = event_task.map(|task| RetainedRpcTask::new(task, pump_stop.clone()));
    let event_flush = event_task.as_ref().map(|_| event_flush_tx);

    // Bridge the extension `UiDialog` mailbox over the rpc channel so a
    // permission-gate extension can ask the client a question mid-run: we emit
    // `{"type":"ui_dialog_request",…}` and answer it from a
    // `{"type":"ui_dialog_response",…}` on stdin. Without this a headless run
    // leaves the mailbox detached and every `UiDialog` call fails.
    ui_dialog.attach();
    let dialog_task = {
        let mailbox = ui_dialog.clone();
        let out = stdout.clone();
        let stop = pump_stop.clone();
        tokio::spawn(async move {
            let mut shown = std::collections::HashSet::new();
            while !stop.is_cancelled() {
                // This **is** the external UI in `--remote` mode, so it claims
                // ownership: the local TUI must not also take from the queue.
                mailbox.set_external_takeover(true);
                // Skip plugin-reserved requests: a remote client takes those
                // through `RelayUiDialog`, and consuming them here would leave
                // the question unanswered on both ends.
                while let Some(request) =
                    mailbox.take_pending_filtered(|r| !mailbox.is_reserved(&r.request_id))
                {
                    let event = crate::remote::protocol::RemoteEvent::UiDialogRequest {
                        request_id: request.request_id.clone(),
                        tool_call_id: request.tool_call_id,
                        ui: request.ui,
                    };
                    write_line(&out, &serde_json::to_value(event).unwrap()).await;
                    shown.insert(request.request_id);
                }
                let closed: Vec<String> = shown
                    .iter()
                    .filter(|id| !mailbox.is_pending(id))
                    .cloned()
                    .collect();
                for request_id in closed {
                    shown.remove(&request_id);
                    let event = crate::remote::protocol::RemoteEvent::UiDialogClosed { request_id };
                    write_line(&out, &serde_json::to_value(event).unwrap()).await;
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
                }
            }
        })
    };

    let dialog_task = RetainedRpcTask::new(dialog_task, pump_stop.clone());

    // A `prompt` runs in a spawned task so this loop keeps reading stdin while
    // the run is active — otherwise `abort` and `ui_dialog_response` could not
    // be delivered mid-run. Only one prompt may be in flight at a time.
    let run_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut run_task: Option<OwnedRpcRun> = None;
    let mut run_cancel = tokio_util::sync::CancellationToken::new();
    let mut cancel_on_exit = false;

    let binding = match rpi_harness::session::SessionValues::load(harness.session()).await {
        Ok(values) => crate::resource_selection::binding_state(&values),
        Err(error) => {
            eprintln!("resource binding read failed: {error}");
            return 1;
        }
    };
    let mut ready = rpc_protocol::ready(message_updates);
    ready["resourceState"] = serde_json::json!({
        "version": 1, "enabled": harness.has_exact_resources(), "command": "get_resource_state",
        "maxSnapshotBytes": crate::resource_selection::MAX_STATE_BYTES,
        "changes": "restart-required", "setActiveTools": if harness.has_exact_resources() { "unsupported" } else { "legacy" },
        "configureExternalTools": if harness.has_exact_resources() { "unsupported" } else { "supported" },
        "automaticPersistenceRecovery": false, "hotApply": false,
        "packageScopes": ["project", "global"], "defaultPackageScope": "project",
        "transition": crate::resource_selection::transition_capability(),
        "binding": binding
    });
    if harness.has_exact_resources() {
        if let Ok(state) = crate::resource_selection::get_resource_state(harness).await {
            for key in [
                "hostId",
                "host",
                "sessionId",
                "selection",
                "binding",
                "transition",
            ] {
                ready["resourceState"][key] = state[key].clone();
            }
        } else {
            ready["resourceState"]["enabled"] = serde_json::json!(false);
        }
    }
    write_line(&stdout, &ready).await;
    setup_timing.finish();

    loop {
        let incoming = tokio::select! {
            _ = writer_failed.cancelled() => {
                cancel_on_exit = true;
                break;
            }
            line = crate::rpc::framing::read_frame(&mut input) => line,
        };
        let line = match incoming {
            Ok(Some(line)) => line,
            Ok(None) => break, // EOF
            Err(error) => {
                eprintln!("[rpi] rpc stdin error: {error}");
                run_cancel.cancel();
                let _ = lane.abort().await;
                external_tools.shutdown();
                writer_failed.cancel();
                cancel_on_exit = true;
                break;
            }
        };
        let line = match std::str::from_utf8(&line) {
            Ok(line) => line.trim(),
            Err(_) => {
                run_cancel.cancel();
                let _ = lane.abort().await;
                external_tools.shutdown();
                writer_failed.cancel();
                cancel_on_exit = true;
                break;
            }
        };
        if line.is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                write_line(
                    &stdout,
                    &rpc_protocol::response(
                        None,
                        "invalid_request",
                        Err(format!("invalid json: {error}")),
                    ),
                )
                .await;
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let kind = msg
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if kind == "get_resource_state" && serde_json::to_vec(&id).map_or(true, |v| v.len() > 4096)
        {
            write_line(
                &stdout,
                &rpc_protocol::response(
                    None,
                    &kind,
                    Err("resource state request id exceeds 4096 bytes".into()),
                ),
            )
            .await;
            continue;
        }
        if kind != "prompt"
            && rpc_protocol::SUPPORTED_COMMANDS.contains(&kind.as_str())
            && serde_json::from_value::<rpc_protocol::Request>(msg.clone()).is_err()
        {
            write_line(
                &stdout,
                &rpc_protocol::response(
                    Some(id.clone()),
                    &kind,
                    Err("invalid command payload".to_string()),
                ),
            )
            .await;
            continue;
        }

        match kind.as_str() {
            "ui_dialog_response" => {
                let request_id = msg
                    .get("requestId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let result = if msg
                    .get("cancelled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    ui_dialog.cancel(&request_id)
                } else {
                    let answer = msg
                        .get("answer")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    ui_dialog.respond(&request_id, answer)
                };
                if msg.get("id").is_some() {
                    let response = match result {
                        Ok(()) => rpc_protocol::response(
                            Some(id),
                            "ui_dialog_response",
                            Ok(Some(
                                serde_json::json!({"accepted":true,"requestId":request_id}),
                            )),
                        ),
                        Err(error) => {
                            rpc_protocol::response(Some(id), "ui_dialog_response", Err(error))
                        }
                    };
                    write_line(&stdout, &response).await;
                } else if let Err(error) = result {
                    eprintln!("[rpi] ui_dialog_response: {error}");
                }
                continue;
            }
            "execute_command" => {
                command_tasks.retain(|task| !task.is_finished());
                let snapshot = oauth_snapshot.clone();
                let request = msg.clone();
                let out = stdout.clone();
                command_tasks.push(RetainedRpcTask::new(
                    tokio::spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            rpc_execute_command(snapshot.as_deref(), &request)
                        })
                        .await
                        .unwrap_or_else(|_| Err("command task failed".into()));
                        write_line(
                            &out,
                            &rpc_protocol::response(Some(id), "execute_command", result.map(Some)),
                        )
                        .await;
                    }),
                    writer_failed.clone(),
                ));
                continue;
            }
            "stop" => {
                cancel_on_exit = true;
                run_cancel.cancel();
                let _ = lane.abort().await;
                external_tools.shutdown();
                write_line(
                    &stdout,
                    &rpc_protocol::response(
                        Some(id.clone()),
                        "stop",
                        Ok(Some(serde_json::json!({"stopped": true}))),
                    ),
                )
                .await;
                break;
            }
            "prompt" | "resume_pending" | "compact" => {
                let resume = kind == "resume_pending";
                let compact = kind == "compact";
                if run_active.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    write_line(
                        &stdout,
                        &rpc_protocol::response(
                            Some(id.clone()),
                            &kind,
                            Err("a run is already active".to_string()),
                        ),
                    )
                    .await;
                    continue;
                }
                if let Some(task) = run_task.take() {
                    let _ = task.await;
                }
                run_cancel = tokio_util::sync::CancellationToken::new();
                owner.run_cancel = run_cancel.clone();
                let cancel_task = run_cancel.clone();
                // Resuming spends the tokens the interrupted run never did, so it
                // only proceeds when recovery actually left a pending run.
                if resume && !lane.has_pending_resume().await {
                    run_active.store(false, std::sync::atomic::Ordering::Release);
                    write_line(
                        &stdout,
                        &rpc_protocol::response(
                            Some(id),
                            "resume_pending",
                            Ok(Some(serde_json::json!({"resumed": false}))),
                        ),
                    )
                    .await;
                    continue;
                }
                let lane_task = Arc::clone(&lane);
                let out_task = stdout.clone();
                let active_task = Arc::clone(&run_active);
                let mailbox_task = ui_dialog.clone();
                let flush_events = event_flush.clone();
                let instructions = msg
                    .get("customInstructions")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                // The run slot includes validation. Reader/control traffic keeps
                // flowing while the single bounded blocking worker decodes.
                // Rejections receive ONE failed prompt response, no acceptance,
                // no model call, no user persistence, and no agent_settled.
                run_task = Some(OwnedRpcRun::new(
                    tokio::spawn(async move {
                        #[cfg(test)]
                        let _finished =
                            crate::rpc::prompt_content::validation_test_hook::for_command(&msg)
                                .map(crate::rpc::prompt_content::validation_test_hook::RunFinished);
                        let validation_timing =
                            RpcPhaseTiming::start(phase_diagnostics, "validation");
                        let content = if resume || compact {
                            Ok(crate::rpc::prompt_content::ParsedPrompt::Legacy(
                                String::new(),
                            ))
                        } else {
                            rpc_validate_prompt(&lane_task, msg, &cancel_task).await
                        };
                        validation_timing.finish();
                        let content = match content {
                            Ok(content) => content,
                            Err(error) => {
                                write_line(
                                    &out_task,
                                    &rpc_protocol::response(Some(id), &kind, Err(error)),
                                )
                                .await;
                                active_task.store(false, std::sync::atomic::Ordering::Release);
                                return;
                            }
                        };
                        write_line(
                            &out_task,
                            &rpc_protocol::response(
                                Some(id),
                                &kind,
                                Ok(Some(serde_json::json!({"accepted": true}))),
                            ),
                        )
                        .await;
                        let outcome_cancel = cancel_task.clone();
                        let execute_timing = RpcPhaseTiming::start(phase_diagnostics, "execute");
                        let result = if compact {
                            rpc_compact_cancellable(&lane_task, instructions, cancel_task).await
                        } else if resume {
                            rpc_resume_cancellable(&lane_task, cancel_task).await
                        } else {
                            rpc_prompt_cancellable(&lane_task, content, cancel_task).await
                        };
                        execute_timing.finish();
                        let flush_timing = RpcPhaseTiming::start(phase_diagnostics, "event_flush");
                        if let Some(flush) = flush_events {
                            let (done, drained) = tokio::sync::oneshot::channel();
                            if flush.send(done).is_ok() {
                                let _ = drained.await;
                            }
                        }
                        flush_timing.finish();
                        let (outcome, aborted, error) = match &result {
                            Ok(value) => {
                                let outcome = value
                                    .get("outcome")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("completed")
                                    .to_string();
                                let error = value
                                    .get("error")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_owned);
                                (outcome.clone(), outcome == "aborted", error)
                            }
                            Err(error) if outcome_cancel.is_cancelled() => {
                                ("aborted".to_string(), true, Some(error.clone()))
                            }
                            Err(ref error) => ("failed".to_string(), false, Some(error.clone())),
                        };
                        // Recovery may leave a continuable run behind an abort, so the
                        // settlement carries what the client needs to offer "continue".
                        let resume_timing =
                            RpcPhaseTiming::start(phase_diagnostics, "pending_resume");
                        let pending_resume = lane_task.has_pending_resume().await;
                        resume_timing.finish();
                        let summary = result
                            .as_ref()
                            .ok()
                            .and_then(|value| value.get("summary"))
                            .cloned();
                        let settled = serde_json::json!({
                            "type": "agent_settled",
                            "runId": result.as_ref().ok().and_then(|value| value.get("runId")),
                            "outcome": outcome,
                            "aborted": aborted,
                            "error": error,
                            "summary": summary,
                            "pendingResume": pending_resume,
                            // Present when this settlement also aborted a run:
                            // the queue the abort reclaimed, so nothing staged
                            // is silently swallowed.
                            "reclaimed": result
                                .as_ref()
                                .ok()
                                .and_then(|value| value.get("reclaimed"))
                                .cloned(),
                        });
                        let enqueue_timing =
                            RpcPhaseTiming::start(phase_diagnostics, "settled_enqueue");
                        write_line(&out_task, &settled).await;
                        enqueue_timing.finish();
                        mailbox_task.cancel_all();
                        active_task.store(false, std::sync::atomic::Ordering::Release);
                    }),
                    run_cancel.clone(),
                ));
                continue;
            }
            _ => {}
        }

        let result = match kind.as_str() {
            "abort" if run_active.load(std::sync::atomic::Ordering::Acquire) => {
                // Keep wrapper cancellation for pre-admission aborts, but signal
                // an admitted harness run before retiring external broker waiters.
                run_cancel.cancel();
                let result = lane.abort().await;
                external_tools.abort();
                // Hand the reclaimed queue back: the caller is a remote UI with
                // an editor to restore into. Discarding it would silently swallow
                // messages the user staged while the run was in flight.
                abort_result_reply(result)
            }
            "abort" => {
                let result = lane.abort().await;
                external_tools.abort();
                abort_result_reply(result)
            }
            "configure_external_tools" => {
                rpc_configure_external_tools(
                    &lane,
                    harness,
                    run_active.load(std::sync::atomic::Ordering::Acquire),
                    &external_tools,
                    &mut external_names,
                    &msg,
                )
                .await
            }
            "external_tool_response" => external_tools
                .respond(&msg)
                .map(|accepted| serde_json::json!({"accepted": accepted})),
            "configure_file_io" => {
                rpc_configure_file_io(
                    &lane,
                    run_active.load(std::sync::atomic::Ordering::Acquire),
                    file_io_router.as_ref(),
                    &mut owner.file_io,
                    &stdout,
                    &msg,
                )
                .await
            }
            // Replies are never run-gated; the tool is waiting on this reader.
            "file_io_response" => {
                // Correlation belongs to the dispatcher, not the strict broker
                // envelope. Move the owned frame and strip ONLY id: arbitrary
                // extra fields must still fail the broker's exact-shape check.
                let mut reply = msg;
                if let Some(object) = reply.as_object_mut() {
                    object.remove("id");
                }
                Ok(serde_json::json!({"accepted": owner.file_io.as_ref()
                    .is_some_and(|client| client.resolve_response(&reply))}))
            }
            "get_resource_state" => {
                if msg.as_object().is_none_or(|o| {
                    o.keys()
                        .any(|k| !["type", "id", "version"].contains(&k.as_str()))
                }) || msg.get("version").is_some_and(|v| v.as_u64() != Some(1))
                {
                    Err("unsupported resource state request/schema (version 1 only)".into())
                } else {
                    crate::resource_selection::get_resource_state(harness).await
                }
            }
            "get_state" => Ok(rpc_get_state(&lane, harness).await),
            "get_commands" => Ok(rpc_get_commands(oauth_snapshot.as_deref())),
            "get_messages" => rpc_get_messages(&lane, harness, &msg).await,
            "get_tool_program_branch" => {
                crate::rpc::tool_program_queries::branch(harness, &msg).await
            }
            "get_message_summaries" => crate::rpc::message_queries::summaries(harness, &msg).await,
            "get_message_window" => crate::rpc::message_queries::window(harness, &msg).await,
            "get_tool_program_children" => {
                crate::rpc::tool_program_queries::children(harness, &msg).await
            }
            "get_tool_program_child_phases" => {
                crate::rpc::tool_program_queries::phases(harness, &msg).await
            }
            "get_tool_program_child_body" => {
                crate::rpc::tool_program_queries::body(harness, &msg).await
            }
            "get_history" => rpc_get_history(&lane, harness, &msg).await,
            "get_session_branch" => rpc_get_session_branch(harness, &msg).await,
            "append_entry" => rpc_append_entry(harness, &msg).await,
            "get_stats" => rpc_get_stats(&lane, harness).await,
            "get_queue" => rpc_get_queue(&lane, harness, &msg).await,
            "queue_message" => rpc_queue_message(&lane, &msg).await,
            "cancel_queued" => rpc_cancel_queued(&lane, &msg).await,
            "clear_queue" => rpc_clear_queue(&lane).await,
            "navigate" => rpc_navigate(&lane, &msg).await,
            "get_available_models" => Ok(rpc_get_available_models(&model_catalog)),
            "get_auth_status" => Ok(rpc_get_auth_status(&model_catalog, oauth_snapshot.as_ref())),
            "get_provider_models" => rpc_get_provider_models(&msg).await,
            "test_provider_config" => rpc_test_provider_config(&msg).await,
            "oauth" => rpc_oauth(&oauth_snapshot, &msg),
            "set_api_key" => rpc_set_api_key(&msg).await,
            "set_provider_config" => rpc_set_provider_config(&msg),
            "update_provider_config" => rpc_update_provider_config(&msg),
            "delete_provider_config" => rpc_delete_provider_config(&msg),
            "set_model" if run_active.load(std::sync::atomic::Ordering::Acquire) => {
                Err("cannot set model while a run or prompt validation is active".into())
            }
            "set_model" => rpc_set_model(&lane, &model_catalog, &msg).await,
            "get_available_thinking_levels" => Ok(rpc_get_available_thinking_levels(&lane).await),
            "set_thinking_level" => rpc_set_thinking_level(&lane, &msg).await,
            "get_available_tools" => Ok(rpc_get_available_tools(&lane, harness).await),
            "set_active_tools" => rpc_set_active_tools(&lane, &msg).await,
            "ping" => Ok(serde_json::json!({"pong": true})),
            other => Err(format!("unknown command type: {other}")),
        };

        let response = rpc_protocol::response(Some(id.clone()), &kind, result.map(Some));
        write_line(&stdout, &response).await;
    }

    // EOF closes the input half of the protocol, not an accepted prompt.
    // There can be no more dialog answers, so release approvals, then await the
    // run before SessionShutdown can unload plugins or the runtime exits.
    ui_dialog.detach();
    if cancel_on_exit {
        run_cancel.cancel();
        let _ = lane.abort().await;
    }
    // EOF makes future external replies impossible. Disable the backend before
    // joining, without cancelling an otherwise healthy model-only run.
    external_tools.shutdown();
    if let Some(client) = owner.file_io.take() {
        client.close();
    }
    for task in command_tasks {
        let _ = task.await;
    }
    let mut exit_code = 0;
    // A broken transport during healthy EOF waiting must still cancel the run.
    if let Some(mut task) = run_task {
        let result = tokio::select! {
            biased;
            result = &mut task => result,
            _ = writer_failed.cancelled() => {
                run_cancel.cancel();
                let _ = lane.abort().await;
                external_tools.shutdown();
                task.await
            }
        };
        if let Err(error) = result {
            eprintln!("[rpi] rpc prompt task failed: {error}");
            exit_code = 1;
        }
    }
    pump_stop.cancel();
    harness.session().set_entry_commit_publisher(None);
    // Accepted writers have finished. Close the last local sender and let the
    // receiver forward queued commits before the output writer is flushed.
    drop(commit_tx);
    if let Err(error) = commit_task.await {
        eprintln!("[rpi] rpc commit event task failed: {error}");
        exit_code = 1;
    }
    let _ = dialog_task.await;
    if let Some(task) = event_task {
        let _ = task.await;
    }
    // All producers have joined; close the queue and flush every queued line.
    drop(stdout);
    match writer_task.await {
        Ok(Ok(())) => {}
        error => {
            eprintln!("[rpi] rpc output failed: {error:?}");
            exit_code = 1;
        }
    }
    owner.armed = false;
    exit_code
}

// All dispatcher-created tasks have a retained join, including writer/pumps
// and extension commands. Root cancellation transfers ownership to cleanup;
// never abort a future which may be joining spawn_blocking work. Cleanup needs
// the Tokio runtime to remain alive; an OS write already in progress and plugin
// callbacks cannot be forcibly preempted safely.
type OwnedRpcRun = RetainedRpcTask<()>;
struct RetainedRpcTask<T: Send + 'static> {
    task: Option<tokio::task::JoinHandle<T>>,
    cancel: tokio_util::sync::CancellationToken,
}
impl<T: Send + 'static> RetainedRpcTask<T> {
    fn new(task: tokio::task::JoinHandle<T>, cancel: tokio_util::sync::CancellationToken) -> Self {
        Self {
            task: Some(task),
            cancel,
        }
    }
    fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(|task| task.is_finished())
    }
}
impl<T: Send + 'static> std::future::Future for RetainedRpcTask<T> {
    type Output = Result<T, tokio::task::JoinError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let result = std::future::Future::poll(
            std::pin::Pin::new(self.task.as_mut().expect("RPC task already joined")),
            cx,
        );
        if result.is_ready() {
            self.task.take();
        }
        result
    }
}
impl<T: Send + 'static> Drop for RetainedRpcTask<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Pruning a completed command must not cancel a healthy transport.
            if !task.is_finished() {
                self.cancel.cancel();
            }
            tokio::spawn(async move {
                if let Err(error) = task.await {
                    eprintln!("[rpi] cancelled RPC task cleanup failed: {error}");
                }
            });
        }
    }
}

// Synchronous cleanup survives cancellation/drop of the RPC owner future.
// The run is signalled before retiring waiters, preserving aborted settlement.
struct RpcOwner<'a> {
    harness: &'a AgentHarness,
    file_io: Option<crate::rpc::file_io::FileIOClient>,
    backend: Arc<crate::rpc::external_tools::ExternalTools>,
    run_cancel: tokio_util::sync::CancellationToken,
    pump_stop: tokio_util::sync::CancellationToken,
    failed: tokio_util::sync::CancellationToken,
    mailbox: rpi_extensions::UiDialogMailbox,
    armed: bool,
}
impl Drop for RpcOwner<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.run_cancel.cancel();
            if let Some(client) = self.file_io.take() {
                client.close();
            }
            self.backend.shutdown();
            self.failed.cancel();
            self.pump_stop.cancel();
            self.mailbox.detach();
            self.harness.session().set_entry_commit_publisher(None);
        }
    }
}

fn rpc_get_commands(snapshot: Option<&rpi_extensions::RegistrySnapshot>) -> serde_json::Value {
    let commands: Vec<_> = snapshot
        .into_iter()
        .flat_map(|s| s.commands())
        .map(
            |command| serde_json::json!({"name": command.name, "description": command.description}),
        )
        .collect();
    serde_json::json!({"commands": commands})
}

fn rpc_execute_command(
    snapshot: Option<&rpi_extensions::RegistrySnapshot>,
    request: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let name = request
        .get("command")
        .and_then(|v| v.as_str())
        .filter(|name| !name.trim_start_matches('/').is_empty())
        .ok_or("command must be a non-empty string")?;
    let args = match request.get("args") {
        None => "",
        Some(value) => value.as_str().ok_or("args must be a string")?,
    };
    let command = snapshot
        .into_iter()
        .flat_map(|s| s.commands())
        .find(|c| c.name.trim_start_matches('/') == name.trim_start_matches('/'))
        .ok_or_else(|| format!("unknown command: {name}"))?;
    let input = serde_json::json!({"command": name, "args": args}).to_string();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out = rpi_plugin_sdk::StbString::empty();
        let rc = (command.handler)(
            rpi_plugin_sdk::StbStringRef::from_str(&input),
            &mut out,
            command.user_data,
        );
        // SAFETY: the callback transfers its output using the host allocator,
        // matching the TUI command invocation contract.
        let out = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(
                out,
                Some(rpi_extensions::host_free_string),
            )
        };
        if rc != 0 {
            return Err(format!("command failed: {name} (code {rc})"));
        }
        serde_json::from_str(&out.to_string_lossy())
            .map_err(|_| format!("command returned invalid JSON: {name}"))
    }))
    .unwrap_or_else(|_| Err(format!("command panicked: {name}")))
}

async fn drain_rpc_events(
    out: &RpcWriter,
    events: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
) {
    loop {
        match events.try_recv() {
            Ok(event) => write_agent_event(out, &event).await,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

/// Admission owns and drains its blocking decoder even when cancellation wins.
/// Bounds are per prompt, and the single run slot bounds workers to one.
async fn rpc_validate_prompt(
    lane: &Arc<dyn AgentLane>,
    command: serde_json::Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<crate::rpc::prompt_content::ParsedPrompt, String> {
    use crate::rpc::prompt_content::parse_prompt_content;
    if cancel.is_cancelled() {
        return Err("Operation aborted during prompt validation".into());
    }
    // Capture the model before decoding. Client set_model is excluded until
    // settlement. The identity recheck below is not a model lock: extensions
    // may still mutate the harness after it, pending a harness-owned run API.
    let model = lane.get_model().await.map_err(|e| e.to_string())?;
    let mut worker = tokio::task::spawn_blocking(move || parse_prompt_content(&command));
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            // abort() cannot preempt an active spawn_blocking decoder. Await it;
            // never drop/detach its JoinHandle and never persist its result.
            let _ = worker.await;
            return Err("Operation aborted during prompt validation".into());
        }
        result = &mut worker => result.map_err(|_| "prompt validation worker failed".to_string())?,
    }?;
    if cancel.is_cancelled() {
        return Err("Operation aborted during prompt validation".into());
    }
    if result.has_images() {
        let current = lane.get_model().await.map_err(|e| e.to_string())?;
        if current.id != model.id || current.provider != model.provider || current.api != model.api
        {
            return Err("model changed during image prompt validation; retry while idle".into());
        }
        if !model.input.contains(&rpi_ai::types::InputModality::Image)
            || !current.input.contains(&rpi_ai::types::InputModality::Image)
        {
            return Err("selected model does not support image input".into());
        }
    }
    Ok(result)
}

async fn rpc_configure_file_io(
    lane: &Arc<dyn AgentLane>,
    run_active: bool,
    router: Option<&Arc<rpi_tools::file_io::FileIORouter>>,
    owner: &mut Option<crate::rpc::file_io::FileIOClient>,
    writer: &RpcWriter,
    command: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use rpi_tools::file_io::FileIOCapabilities;
    if run_active
        || lane
            .snapshot()
            .await
            .map_err(|e| e.to_string())?
            .active
            .is_some()
    {
        return Err("cannot configure file I/O while a run or prompt validation is active".into());
    }
    let read_text = command
        .get("readText")
        .and_then(serde_json::Value::as_bool)
        .ok_or("readText must be a boolean")?;
    let write_text = command
        .get("writeText")
        .and_then(serde_json::Value::as_bool)
        .ok_or("writeText must be a boolean")?;
    let router = router.ok_or("this embedded harness has no attached built-in file I/O router")?;
    // Close the old unique owner before installing a fresh weak backend. After
    // transport loss the selected router remains client-authoritative: no fallback.
    if let Some(old) = owner.take() {
        old.close();
    }
    let next =
        (read_text || write_text).then(|| crate::rpc::file_io::FileIOClient::new(writer.clone()));
    router
        .configure(
            FileIOCapabilities {
                read_text,
                write_text,
            },
            next.as_ref().map(|c| c.backend()),
        )
        .await
        .map_err(|e| e.to_string())?;
    *owner = next;
    Ok(serde_json::json!({"readText": read_text, "writeText": write_text}))
}

/// Cancellation is delivered even if abort/stop arrives before prompt_text has
/// registered a run. Once active, await its normal aborted outcome and cleanup.
async fn rpc_prompt_cancellable(
    lane: &Arc<dyn AgentLane>,
    content: crate::rpc::prompt_content::ParsedPrompt,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, String> {
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let prompt = rpc_prompt_content(lane, content);
    tokio::pin!(prompt);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            // `abort()` reclaims the queue atomically. The entries are not
            // dropped: they ride back on the settlement so the client can
            // restore what the user staged while the run was in flight.
            match lane.abort().await {
                Ok(reclaimed) => prompt.await.map(|mut value| {
                    if let Some(object) = value.as_object_mut() {
                        object.insert(
                            "reclaimed".to_string(),
                            reclaimed_payload(&reclaimed),
                        );
                    }
                    value
                }),
                Err(_) => Err("Operation aborted".to_string()),
            }
        }
        result = &mut prompt => result,
    }
}

/// Reclaimed queue entries from an abort, for replies that also settle a run.
fn reclaimed_payload(result: &rpi_harness::agent_harness::AbortResult) -> serde_json::Value {
    serde_json::json!({
        "steer": crate::extensions_actions::queued_message_payloads(&result.steer),
        "followUp": crate::extensions_actions::queued_message_payloads(&result.follow_up),
    })
}

/// Compact the branch, unless the caller cancels first.
///
/// Compaction is its own operation kind in the harness: it spends a provider
/// call on the summary, so it takes the same single-run slot and reports the
/// same settlement shape a prompt does.
async fn rpc_compact_cancellable(
    lane: &Arc<dyn AgentLane>,
    instructions: Option<String>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, String> {
    let compact = async {
        match lane.compact(instructions.as_deref()).await {
            Ok(result) => Ok(compaction_json(&result)),
            Err(error) => Err(error.to_string()),
        }
    };
    tokio::pin!(compact);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = lane.abort().await;
            compact.await
        }
        result = &mut compact => result,
    }
}

/// Reduce a compaction result to what a client needs: the verdict, and a line
/// a transcript can print.
fn compaction_json(result: &CompactionResult) -> serde_json::Value {
    let mut value = match &result.outcome {
        CompactionOutcome::Completed { leaf_id, entry } => {
            let (tokens_before, retained) = match entry {
                rpi_harness::session::types::Entry::Compaction(compaction) => {
                    (compaction.tokens_before, compaction.retained_tail.len())
                }
                _ => (0, 0),
            };
            serde_json::json!({
                "outcome": "compacted",
                "leafId": leaf_id,
                "tokensBefore": tokens_before,
                "retainedMessages": retained,
                "summary": format!(
                    "Conversation compacted · {} tokens before, {} messages kept.",
                    tokens_before, retained
                ),
            })
        }
        CompactionOutcome::Declined { leaf_id } => serde_json::json!({
            "outcome": "declined",
            "leafId": leaf_id,
            "summary": "Nothing to compact yet.",
        }),
        CompactionOutcome::Aborted { leaf_id } => serde_json::json!({
            "outcome": "aborted",
            "leafId": leaf_id,
            "summary": "Compaction aborted.",
        }),
        CompactionOutcome::Failed { leaf_id, error } => serde_json::json!({
            "outcome": "failed",
            "leafId": leaf_id,
            "error": error.message,
            "summary": format!("Compaction failed: {}", error.message),
        }),
    };
    value["runId"] = serde_json::json!(result.run_id);
    value
}

/// Drive the run recovery left behind, unless the caller cancels first.
///
/// `Ok(None)` from the lane means there was nothing to resume: the run is
/// reported as `skipped` so the client can settle without inventing an outcome.
async fn rpc_resume_cancellable(
    lane: &Arc<dyn AgentLane>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, String> {
    let resume = async {
        match lane.resume_pending().await {
            Ok(Some(result)) => {
                let (outcome, final_text, error) = rpc_outcome_str(&result.outcome);
                Ok(serde_json::json!({
                    "runId": result.run_id,
                    "outcome": outcome,
                    "finalText": final_text,
                    "error": error,
                    "resumed": true,
                }))
            }
            Ok(None) => Ok(serde_json::json!({"outcome": "skipped", "resumed": false})),
            Err(error) => Err(error.to_string()),
        }
    };
    tokio::pin!(resume);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = lane.abort().await;
            resume.await
        }
        result = &mut resume => result,
    }
}

fn rpc_outcome_str(outcome: &HarnessRunOutcome) -> (&'static str, Option<String>, Option<String>) {
    match outcome {
        HarnessRunOutcome::Completed { final_message, .. } => {
            ("completed", Some(assistant_text(final_message)), None)
        }
        HarnessRunOutcome::Aborted { final_message, .. } => {
            ("aborted", Some(assistant_text(final_message)), None)
        }
        // A failed run can end with nothing to say, which is exactly when the
        // reason matters: rate limits and provider errors live here.
        HarnessRunOutcome::Failed {
            final_message,
            error,
            ..
        } => (
            "failed",
            final_message.as_ref().map(assistant_text),
            Some(error.message.clone()),
        ),
        HarnessRunOutcome::Suspended { .. } => ("suspended", None, None),
    }
}

async fn rpc_prompt_content(
    lane: &Arc<dyn AgentLane>,
    content: crate::rpc::prompt_content::ParsedPrompt,
) -> Result<serde_json::Value, String> {
    use crate::rpc::prompt_content::ParsedPrompt;
    let result = match content {
        ParsedPrompt::Legacy(text) => lane.prompt_text(&text, Vec::new()).await,
        ParsedPrompt::Blocks(blocks) => {
            lane.prompt_message(rpi_agent::AgentMessage::User(UserMessage::new(
                rpi_ai::types::UserContent::Blocks(blocks),
                rpc_now_ms(),
            )))
            .await
        }
    }
    .map_err(|e| e.to_string())?;
    let (outcome, final_text, error) = rpc_outcome_str(&result.outcome);
    Ok(
        serde_json::json!({"runId":result.run_id, "outcome": outcome, "finalText": final_text, "error": error}),
    )
}

async fn rpc_get_state(lane: &Arc<dyn AgentLane>, harness: &AgentHarness) -> serde_json::Value {
    let model = lane.get_model().await.ok();
    let thinking = lane
        .get_thinking_level()
        .await
        .ok()
        .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null));
    let active_tools = if harness.has_exact_resources() {
        harness
            .execution_context()
            .ok()
            .and_then(|v| v.get("tools").cloned())
            .unwrap_or_else(|| serde_json::json!([]))
    } else {
        serde_json::json!(lane.get_active_tools().await.unwrap_or_default())
    };
    let leaf_id = lane.get_leaf_id().await.ok().flatten();
    let context = rpc_context_usage(lane, harness).await;
    let active_operation = lane.active_operation().await.ok().flatten().map(|active| {
        serde_json::json!({
            "runId": active.run_id,
            "lane": active.lane,
            "kind": format!("{:?}", active.kind).to_lowercase(),
        })
    });
    let queued = lane.queued_messages().await.unwrap_or_default();
    let stats = rpc_get_stats(lane, harness).await.ok();
    serde_json::json!({
        "modelId": model.as_ref().map(|model| model.id.clone()),
        "model": model.as_ref().map(|model| model.id.clone()),
        "provider": model.as_ref().map(|model| model.provider.clone()),
        "thinkingLevel": thinking,
        "activeTools": active_tools,
        "leafId": leaf_id,
        "context": context,
        "sessionId": harness.session().storage().metadata().id,
        "activeOperation": active_operation,
        "queue": {
            // Text-only view: these consumers render a single-line pending row.
            // Attachments travel in the `Abort` / `ClearQueue` replies, which
            // keep the per-message pairing.
            //
            // Each queue is serialized on its own. `texts()` chains steering and
            // follow-up, so using it here would put follow-up texts in the
            // `steering` field and duplicate them in `followUp`.
            "steering": queued
                .steering
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            "followUp": queued.follow_up.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            "steeringCount": queued.steering.len(),
            "followUpCount": queued.follow_up.len(),
        },
        "stats": stats,
    })
}

fn rpc_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn rpc_message_text(value: &serde_json::Value) -> String {
    let Some(content) = value.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(|text| text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn rpc_get_history(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let limit = msg
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let after_seq = msg.get("afterSeq").and_then(serde_json::Value::as_u64);
    let before_seq = msg.get("beforeSeq").and_then(serde_json::Value::as_u64);
    let page = harness
        .session()
        .record_page(&rpi_harness::session::types::RecordPageQuery {
            lane: lane.name().to_owned(),
            after_seq,
            before_seq,
            limit,
            from_end: before_seq.is_some() || after_seq.is_none(),
        })
        .await
        .map_err(|error| error.to_string())?;
    let records = page.records;
    let total = page.total;
    let has_more = total > records.len();
    let first_seq = records.first().map(LaneRecord::seq);
    let last_seq = records.last().map(LaneRecord::seq);
    let open_operations = harness
        .session()
        .find_open_operations(lane.name(), Some(50))
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "records": records.iter().map(|record| serde_json::to_value(record).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>(),
        "total": total,
        "limit": limit,
        "afterSeq": after_seq,
        "beforeSeq": before_seq,
        "firstSeq": first_seq,
        "lastSeq": last_seq,
        "hasMore": has_more,
        "leafId": lane.get_leaf_id().await.ok().flatten(),
        "openOperations": open_operations.iter().map(|record| serde_json::to_value(record).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>(),
    }))
}

async fn rpc_get_session_branch(
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // Envelope fields are not query arguments. Everything else is validated by
    // the same strict parser used by the runtime action.
    let mut args = msg.clone();
    if let Some(args) = args.as_object_mut() {
        args.remove("type");
        args.remove("id");
    }
    let request = branch_entry_query(&args)?;
    let (entries, session_id, leaf_id) = branch_snapshot(harness, &request).await?;
    let entries = entries
        .iter()
        .map(|entry| serde_json::to_value(entry).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({
        "entries": entries,
        "count": entries.len(),
        "filtered": true,
        "sessionId": session_id,
        "leafId": leaf_id,
        "scopedAppend": true,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BranchEntryQuery {
    pub custom_types: Vec<String>,
    pub order: EntryOrder,
    pub limit: Option<usize>,
    pub after_seq: Option<u64>,
}

impl BranchEntryQuery {
    fn entry_queries(&self) -> Vec<EntryQuery> {
        // A global top-L entry must be within its type's top L under the same
        // order/cursor. Bound cloned payloads before the global merge as well.
        let per_query_limit = self.limit;
        self.custom_types
            .iter()
            .map(|custom_type| EntryQuery {
                entry_type: Some("custom"),
                custom_type: Some(custom_type.clone()),
                order: Some(self.order),
                limit: per_query_limit,
                cursor: self.after_seq.map(|after_seq| EntryCursor { after_seq }),
                ..Default::default()
            })
            .collect()
    }
}

pub(crate) async fn branch_snapshot(
    harness: &AgentHarness,
    request: &BranchEntryQuery,
) -> Result<(Vec<Entry>, String, Option<String>), String> {
    // Pin both the storage binding and branch tip once. Session handles follow
    // harness session switches, and resolving the live leaf per type can merge
    // entries from different branches while a query is in flight.
    let storage = harness.session().storage();
    let leaf_id = storage
        .get_lanes()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|pointer| pointer.lane == "main")
        .ok_or_else(|| "Lane not found: main".to_string())?
        .leaf_id;
    let session_id = storage.metadata().id;
    let Some(start) = leaf_id.clone() else {
        return Ok((Vec::new(), session_id, None));
    };
    let bounds = BranchBounds {
        start: Some(start.clone()),
        ..Default::default()
    };
    let mut entries = Vec::new();
    for query in request.entry_queries() {
        entries.extend(
            storage
                .find_entries_on_branch(&query, &bounds, &start)
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    if request.custom_types.len() > 1 {
        let mut seen = HashSet::with_capacity(entries.len());
        entries.retain(|entry| seen.insert(entry.id().to_owned()));
        match request.order {
            EntryOrder::OldestFirst => entries.sort_by_key(Entry::seq),
            EntryOrder::NewestFirst => entries.sort_by_key(|entry| Reverse(entry.seq())),
        }
        if let Some(limit) = request.limit {
            entries.truncate(limit);
        }
    }
    Ok((entries, session_id, leaf_id))
}

/// Canonical custom-entry query. Reject unknown/legacy arguments even when
/// valid modern arguments are also present; never fall back to a full scan.
pub(crate) fn branch_entry_query(msg: &serde_json::Value) -> Result<BranchEntryQuery, String> {
    let args = msg
        .as_object()
        .ok_or("get_session_branch: query must be an object")?;
    for key in args.keys() {
        if !matches!(key.as_str(), "customTypes" | "afterSeq" | "limit" | "order") {
            return Err(format!(
                "get_session_branch: unknown query argument `{key}`"
            ));
        }
    }
    let values = args
        .get("customTypes")
        .and_then(serde_json::Value::as_array)
        .filter(|values| !values.is_empty())
        .ok_or(
            "get_session_branch: `customTypes` is required and must be a non-empty string array",
        )?;
    let mut custom_types = Vec::with_capacity(values.len());
    for value in values {
        let custom_type = value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or("get_session_branch: every `customTypes` item must be a non-empty string")?;
        if !custom_types.iter().any(|existing| existing == custom_type) {
            custom_types.push(custom_type.to_owned());
        }
    }
    let limit = match msg.get("limit") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            let limit = value
                .as_u64()
                .filter(|limit| *limit > 0)
                .ok_or("get_session_branch: `limit` must be a positive integer")?;
            Some(usize::try_from(limit).map_err(|_| "get_session_branch: `limit` is too large")?)
        }
    };
    let order = match msg.get("order") {
        None => EntryOrder::OldestFirst,
        Some(serde_json::Value::String(value)) if value == "oldestFirst" => EntryOrder::OldestFirst,
        Some(serde_json::Value::String(value)) if value == "newestFirst" => EntryOrder::NewestFirst,
        Some(_) => {
            return Err("get_session_branch: `order` must be oldestFirst or newestFirst".into())
        }
    };
    let after_seq = match msg.get("afterSeq") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or("get_session_branch: `afterSeq` must be a non-negative integer")?,
        ),
    };
    Ok(BranchEntryQuery {
        custom_types,
        order,
        limit,
        after_seq,
    })
}

async fn rpc_append_entry(
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let custom_type = msg
        .get("customType")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "append_entry requires a non-empty string `customType`".to_string())?;
    crate::resource_selection::validate_public_entry(custom_type, msg.get("data"))?;
    let entry_id = if let Some(expected) = msg.get("expected") {
        let guard = serde_json::from_value(expected.clone())
            .map_err(|error| format!("invalid expected guard: {error}"))?;
        harness
            .append_scoped_entry(
                &guard,
                rpi_harness::agent_harness::ScopedEntry::Custom {
                    custom_type: custom_type.to_owned(),
                    data: msg.get("data").cloned(),
                },
            )
            .await
    } else {
        harness
            .append_custom_entry(custom_type, msg.get("data").cloned())
            .await
    }
    .map_err(|error| error.to_string())?;
    let mut response = serde_json::json!({"entryId": entry_id, "customType": custom_type});
    if msg.get("expected").is_some() {
        response["scoped"] = serde_json::json!(true);
    }
    Ok(response)
}

async fn rpc_get_stats(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
) -> Result<serde_json::Value, String> {
    let stats = lane.get_stats().await.map_err(|error| error.to_string())?;
    // Filter before cloning/hydrating; legacy streaming progress is unrelated.
    let mut records = Vec::new();
    for record_type in [
        "operation_started",
        "operation_finished",
        "retry_pending",
        "usage",
    ] {
        records.extend(
            harness
                .session()
                .find_records(&RecordQuery {
                    lane: Some(lane.name().to_string()),
                    record_type: Some(record_type),
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    records.sort_unstable_by_key(LaneRecord::seq);
    let mut operation_counts = std::collections::BTreeMap::<String, u64>::new();
    for record in &records {
        let kind = match record {
            LaneRecord::OperationStarted(started) => {
                let key = started.intent.kind();
                *operation_counts.entry(key.to_string()).or_default() += 1;
                if matches!(
                    &started.intent,
                    rpi_harness::session::types::OperationIntent::Run {
                        resume_data: Some(_),
                        ..
                    }
                ) {
                    *operation_counts.entry("resume".to_string()).or_default() += 1;
                }
                continue;
            }
            LaneRecord::OperationFinished(_) => "operation_finished",
            LaneRecord::RetryPending(_) => "retry_pending",
            LaneRecord::Usage(_) => "usage",
            _ => continue,
        };
        *operation_counts.entry(kind.to_string()).or_default() += 1;
    }
    let count = |name: &str| operation_counts.get(name).copied().unwrap_or_default();
    let model = lane.get_model().await.ok();
    let input = stats.cached_tokens + stats.uncached_tokens;
    Ok(serde_json::json!({
        "messageCount": stats.message_count,
        "operationCount": count("operation_finished"),
        "retryCount": count("retry_pending"),
        "compactionCount": count("compaction"),
        "resumeCount": count("resume"),
        "operationCounts": operation_counts,
        "provider": model.as_ref().map(|value| value.provider.clone()),
        "modelId": model.as_ref().map(|value| value.id.clone()),
        "cachedTokens": stats.cached_tokens,
        "uncachedTokens": stats.uncached_tokens,
        "totalTokens": stats.total_tokens,
        "costTotal": stats.cost_total,
        "cacheHitRatio": if input > 0 { Some(stats.cached_tokens as f64 / input as f64) } else { None::<f64> },
    }))
}

async fn rpc_get_queue(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let include_completed = msg
        .get("includeCompleted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // Filter before cloning/hydrating; legacy streaming progress is unrelated.
    let mut records = Vec::new();
    for record_type in ["queue_enqueued", "queue_cancelled"] {
        records.extend(
            harness
                .session()
                .find_records(&RecordQuery {
                    lane: Some(lane.name().to_string()),
                    record_type: Some(record_type),
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    records.sort_unstable_by_key(LaneRecord::seq);
    let mut cancelled = std::collections::HashMap::<String, u64>::new();
    for record in &records {
        if let LaneRecord::QueueCancelled(record) = record {
            cancelled.insert(record.entry_id.clone(), record.base.seq);
        }
    }
    let mut items = Vec::new();
    for record in records {
        let LaneRecord::QueueEnqueued(record) = record else {
            continue;
        };
        let Some(entry_id) = record.target.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        let (status, completed_seq) = if let Some(seq) = cancelled.get(entry_id) {
            ("cancelled", Some(*seq))
        } else if let Some(entry) = harness.get_entry(entry_id).await.ok().flatten() {
            ("consumed", Some(entry.base().seq))
        } else {
            ("queued", None)
        };
        if status != "queued" && !include_completed {
            continue;
        }
        let message = record
            .target
            .get("message")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        items.push(serde_json::json!({
            "entryId": entry_id,
            "queue": record.queue,
            "status": status,
            "text": rpc_message_text(&message),
            "message": message,
            "runId": record.run_id,
            "seq": record.base.seq,
            "completedSeq": completed_seq,
        }));
    }
    Ok(
        serde_json::json!({"items": items, "count": items.len(), "includeCompleted": include_completed}),
    )
}

async fn rpc_queue_message(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let text = msg
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| "queue_message requires a non-empty string `message`".to_string())?;
    let message = rpi_agent::message::AgentMessage::User(UserMessage::new(text, rpc_now_ms()));
    let queue = msg
        .get("deliverAs")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("followUp");
    let result = match queue {
        "steer" => lane.steer(message).await,
        "followUp" | "follow_up" => lane.follow_up(message).await,
        "nextTurn" | "next_run" => lane.next_run(message).await,
        other => return Err(format!("unknown queue kind: {other}")),
    }
    .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "accepted": true,
        "entryId": result.entry_id,
        "queue": queue,
    }))
}

async fn rpc_cancel_queued(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let entry_id = msg
        .get("entryId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "cancel_queued requires `entryId`".to_string())?;
    let result = lane
        .cancel_queued(entry_id)
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "entryId": entry_id,
        "outcome": format!("{:?}", result.outcome).to_lowercase(),
    }))
}

/// The `abort` reply. Reclaimed queue entries travel back as `{text, images}`
/// so a remote UI can restore them instead of losing staged input.
fn abort_response(result: &rpi_harness::agent_harness::AbortResult) -> serde_json::Value {
    serde_json::json!({
        "aborted": true,
        "steer": crate::extensions_actions::queued_message_payloads(&result.steer),
        "followUp": crate::extensions_actions::queued_message_payloads(&result.follow_up),
    })
}

/// Map a lane abort to its RPC reply.
///
/// A stop button is idempotent: an already-idle lane reports success with
/// nothing reclaimed instead of surfacing `NoActiveRun`. Any other error is
/// still reported.
fn abort_result_reply(
    result: Result<rpi_harness::agent_harness::AbortResult, rpi_harness::result::HarnessError>,
) -> Result<serde_json::Value, String> {
    match result {
        Ok(result) => Ok(abort_response(&result)),
        Err(rpi_harness::result::HarnessError::NoActiveRun { .. }) => {
            Ok(abort_response(&rpi_harness::agent_harness::AbortResult {
                run_id: String::new(),
                steer: Vec::new(),
                follow_up: Vec::new(),
            }))
        }
        Err(error) => Err(error.to_string()),
    }
}

async fn rpc_clear_queue(lane: &Arc<dyn AgentLane>) -> Result<serde_json::Value, String> {
    let result = lane
        .clear_queue()
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "cleared": result.steering.len() + result.follow_up.len(),
        // Independent per-queue views; `texts()` would mix the two and
        // duplicate follow-up entries into `steering`.
        "steering": result
            .steering
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        "followUp": result.follow_up.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
    }))
}

async fn rpc_navigate(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let target_id = msg
        .get("targetId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty());
    if lane
        .snapshot()
        .await
        .map_err(|error| error.to_string())?
        .active
        .is_some()
    {
        return Err("cannot navigate while an operation is active".to_string());
    }
    let result = lane
        .navigate_tree(target_id, false, None, None)
        .await
        .map_err(|error| error.to_string())?;
    let (outcome, leaf_id, error) = match result.outcome {
        rpi_harness::agent_harness::NavigationOutcome::Completed { new_leaf_id, .. } => {
            ("completed", new_leaf_id, None)
        }
        rpi_harness::agent_harness::NavigationOutcome::Declined { leaf_id } => {
            ("declined", leaf_id, None)
        }
        rpi_harness::agent_harness::NavigationOutcome::Aborted { leaf_id } => {
            ("aborted", leaf_id, None)
        }
        rpi_harness::agent_harness::NavigationOutcome::Failed { leaf_id, error } => {
            ("failed", leaf_id, Some(error.message))
        }
    };
    Ok(serde_json::json!({
        "runId": result.run_id,
        "outcome": outcome,
        "leafId": leaf_id,
        "error": error,
    }))
}

/// How full the model's context is, measured the way compaction measures it.
///
/// The branch is projected into model input first (that is what applies a
/// compaction boundary), so the number matches what the next provider call will
/// actually send rather than how many messages the transcript happens to hold.
async fn rpc_context_usage(lane: &Arc<dyn AgentLane>, harness: &AgentHarness) -> serde_json::Value {
    use rpi_harness::compaction::{build_session_context, estimate_context_tokens};
    use rpi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery};

    let estimate = match harness.context_usage().await {
        Ok(Some(estimate)) => estimate,
        Ok(None) => {
            // Legacy/custom backends keep the original default projection.
            let entries = harness
                .find_entries_on_branch(
                    &EntryQuery {
                        order: Some(EntryOrder::OldestFirst),
                        ..Default::default()
                    },
                    &BranchBounds::default(),
                )
                .await
                .unwrap_or_default();
            estimate_context_tokens(&build_session_context(&entries).messages)
        }
        // State polling has always been best effort: a read failure yielded
        // an empty estimate. Do not retry a supported backend by reading all
        // bodies again, or turn its failure into an "unsupported" capability.
        Err(_) => estimate_context_tokens(&[]),
    };
    let window = lane
        .get_model()
        .await
        .ok()
        .map(|model| model.context_window)
        .unwrap_or(0);
    let settings = harness.compaction_settings();
    let window = window as i64;
    let threshold = window - settings.reserve_tokens;
    let percent = if window > 0 {
        (estimate.tokens as f64) * 100.0 / (window as f64)
    } else {
        0.0
    };
    serde_json::json!({
        "tokens": estimate.tokens,
        "window": window,
        "threshold": threshold,
        "percent": (percent * 10.0).round() / 10.0,
        "reserveTokens": settings.reserve_tokens,
        "keepRecentTokens": settings.keep_recent_tokens,
        "compactionEnabled": settings.enabled,
        // False once a real provider usage report backs the number; until then
        // it is a character-based estimate.
        "estimated": estimate.last_usage_index.is_none(),
        "trailingTokens": estimate.trailing_tokens,
    })
}

/// The window `get_messages` returns, expressed as a slice range plus whether an
/// older page still exists. `before` is the index of the `beforeId` entry.
fn page_window(
    total: usize,
    limit: usize,
    before: Option<usize>,
) -> (std::ops::Range<usize>, bool) {
    match before {
        Some(index) => {
            let start = index.saturating_sub(limit);
            (start..index, start > 0)
        }
        None => {
            let skip = total.saturating_sub(limit);
            (skip..total, skip > 0)
        }
    }
}

/// Read-only snapshot of the current branch, oldest first. Messages are
/// projected with the same `AgentMessage` encoding the event stream uses, so a
/// client can render a transcript without understanding session entries.
async fn rpc_get_messages(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use rpi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery};

    const DEFAULT_LIMIT: usize = 400;
    let limit = msg
        .get("limit")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_LIMIT)
        .max(1);
    // `beforeId` pages backwards: the returned window ends immediately before
    // the entry with that id, so a client can walk a long branch one page at a
    // time instead of always re-reading the tail.
    let before_id = msg
        .get("beforeId")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let selected = harness
        .transcript_page(&rpi_harness::session::types::TranscriptPageQuery {
            limit,
            before_id: before_id.clone(),
        })
        .await
        .map_err(|error| error.to_string())?;
    let (window, total, has_more, leaf_id) = if let Some(page) = selected {
        // The storage pins/counts the branch using metadata and hydrates only
        // this window. Use its snapshot leaf, not a later lane position.
        (
            rpc_project_transcript(page.entries)?,
            page.total,
            page.has_more,
            page.leaf_id,
        )
    } else {
        // Legacy/custom storage keeps the original projection and error rules.
        let entries = harness
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut items = rpc_project_transcript(entries)?;
        let total = items.len();
        let before_index = match &before_id {
            Some(before) => Some(
                items
                    .iter()
                    .position(|(id, _)| id == before)
                    .ok_or_else(|| format!("beforeId not found: {before}"))?,
            ),
            None => None,
        };
        let (range, has_more) = page_window(total, limit, before_index);
        let window = items.drain(range).collect::<Vec<_>>();
        (
            window,
            total,
            has_more,
            lane.get_leaf_id().await.ok().flatten(),
        )
    };
    let first_id = window.first().map(|(id, _)| id.clone());
    let last_id = window.last().map(|(id, _)| id.clone());
    let messages = window
        .into_iter()
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    let pending_resume = lane.has_pending_resume().await;
    Ok(serde_json::json!({
        "messages": messages,
        "total": total,
        "truncated": has_more,
        "limit": limit,
        "firstId": first_id,
        "lastId": last_id,
        "hasMore": has_more,
        "beforeId": before_id,
        "leafId": leaf_id,
        "pendingResume": pending_resume,
    }))
}

fn rpc_project_transcript(
    entries: Vec<rpi_harness::session::types::Entry>,
) -> Result<Vec<(String, serde_json::Value)>, String> {
    use rpi_harness::session::types::Entry;
    let mut items = Vec::new();
    for entry in entries {
        match entry {
            Entry::Message(entry) => {
                let mut value = serde_json::to_value(&entry.message).map_err(|e| e.to_string())?;
                // Use the session entry ID for stable beforeId cursors.
                value["entryId"] = serde_json::json!(entry.base.id);
                items.push((entry.base.id, value));
            }
            Entry::Compaction(compaction) => items.push((
                compaction.base.id.clone(),
                serde_json::json!({
                    "role": "compaction",
                    "entryId": compaction.base.id,
                    "tokensBefore": compaction.tokens_before,
                    "retainedMessages": compaction.retained_tail.len(),
                    "timestamp": compaction.base.timestamp,
                }),
            )),
            _ => {}
        }
    }
    Ok(items)
}

fn rpc_auth_provider_ids(model_catalog: &[rpi_ai::Model]) -> std::collections::BTreeSet<String> {
    let mut providers =
        std::collections::BTreeSet::from(["anthropic".to_string(), "openai".to_string()]);
    providers.extend(model_catalog.iter().map(|model| model.provider.clone()));
    if let Ok(models) = crate::config::load_models_config() {
        providers.extend(models.providers.keys().cloned());
    }
    providers
}

fn rpc_get_auth_status(
    model_catalog: &[rpi_ai::Model],
    oauth_snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> serde_json::Value {
    let store = crate::config::read_auth().unwrap_or_default();
    let models = crate::config::load_models_config().unwrap_or_default();
    let manifests = oauth_snapshot.map_or_else(Vec::new, |snapshot| {
        snapshot
            .providers()
            .iter()
            .filter_map(|provider| snapshot.provider_manifest(&provider.provider_id).cloned())
            .collect()
    });
    let auth_states = crate::credentials::statuses(&models, &store, &manifests);
    let providers = rpc_auth_provider_ids(model_catalog)
        .into_iter()
        .map(|provider| {
            let credential = store.get(&provider);
            let state = auth_states.get(&provider).cloned().unwrap_or(
                rpi_runtime::credentials::AuthState {
                    status: rpi_runtime::credentials::AuthStatus::Missing,
                    source: crate::credentials::CredentialSource::Missing,
                },
            );
            let oauth = matches!(credential, Some(crate::config::Credential::Oauth { .. }));
            let oauth_available =
                oauth_snapshot.is_some_and(|snapshot| snapshot.oauth_provider(&provider).is_some());
            serde_json::json!({
                "provider": provider,
                "ready": state.is_ready(),
                "source": state.source.as_json_str(),
                "status": state.status,
                "oauth": oauth,
                "oauthAvailable": oauth_available,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({"providers": providers})
}

fn rpc_oauth(
    snapshot: &Option<Arc<rpi_extensions::RegistrySnapshot>>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let snapshot = snapshot
        .as_ref()
        .ok_or_else(|| "OAuth is unavailable without an extension provider".to_string())?;
    let provider = msg
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "oauth requires a provider".to_string())?;
    let action = msg
        .get("action")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| matches!(*value, "begin" | "exchange" | "refresh" | "revoke"))
        .ok_or_else(|| "oauth requires a valid action".to_string())?;
    let input = msg
        .get("input")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let raw = rpi_extensions::request_oauth(snapshot, provider, action, input)
        .map_err(|_| "OAuth provider request failed".to_string())?;
    let response = rpi_extensions::OAuthActionResponse::parse(raw)
        .map_err(|_| "OAuth provider returned an invalid response".to_string())?;
    match response.kind {
        rpi_extensions::OAuthResponseKind::Credential => {
            let credential = response
                .credential()
                .map_err(|_| "OAuth provider returned an invalid credential".to_string())?
                .ok_or_else(|| "OAuth provider returned no credential".to_string())?;
            crate::config::upsert_oauth_token(
                provider,
                credential.access,
                credential.refresh,
                credential.expires_at,
            )
            .map_err(|_| "OAuth credentials could not be saved".to_string())?;
            Ok(serde_json::json!({
                "provider": provider,
                "status": "authenticated"
            }))
        }
        rpi_extensions::OAuthResponseKind::Interaction => Ok(serde_json::json!({
            "provider": provider,
            "status": "interaction",
            "display": oauth_display(&response.payload)
        })),
        rpi_extensions::OAuthResponseKind::Success => Ok(serde_json::json!({
            "provider": provider,
            "status": "success"
        })),
        rpi_extensions::OAuthResponseKind::Error => {
            Err("OAuth provider rejected the request".to_string())
        }
    }
}

/// Keep provider-defined OAuth UI data deliberately narrow. In particular,
/// token-like fields never cross the RPC boundary, even if a plugin includes
/// them in an interaction payload by mistake.
fn oauth_display(payload: &serde_json::Value) -> serde_json::Value {
    let display = payload.get("display").unwrap_or(payload);
    let Some(object) = display.as_object() else {
        return serde_json::json!({});
    };
    let mut safe = serde_json::Map::new();
    for key in [
        "title",
        "url",
        "instructions",
        "user_code",
        "verification_uri",
    ] {
        if let Some(value) = object.get(key) {
            safe.insert(key.to_string(), value.clone());
        }
    }
    serde_json::Value::Object(safe)
}

async fn rpc_get_provider_models(msg: &serde_json::Value) -> Result<serde_json::Value, String> {
    let required = |name: &str| {
        msg.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("get_provider_models requires {name}"))
    };
    let api = required("api")?;
    let base_url = required("baseUrl")?;
    let api_key = required("apiKey")?;
    let models = crate::provider_probe::get_models(api, base_url, api_key).await?;
    Ok(serde_json::json!({"models": models}))
}

async fn rpc_test_provider_config(msg: &serde_json::Value) -> Result<serde_json::Value, String> {
    let required = |name: &str| {
        msg.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("test_provider_config requires {name}"))
    };
    let result = crate::provider_probe::test_connection(
        required("api")?,
        required("baseUrl")?,
        required("apiKey")?,
    )
    .await?;
    Ok(serde_json::to_value(result).map_err(|error| error.to_string())?)
}

fn rpc_set_provider_config(msg: &serde_json::Value) -> Result<serde_json::Value, String> {
    let required = |name: &str| {
        msg.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("set_provider_config requires {name}"))
    };
    let provider = required("provider")?;
    let api = required("api")?;
    let base_url = required("baseUrl")?;
    let api_key = required("apiKey")?;
    let model_id = required("modelId")?;
    let model_name = msg.get("modelName").and_then(serde_json::Value::as_str);
    let positive_integer = |name: &str| match msg.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
            .ok_or_else(|| format!("{name} must be a positive integer"))
            .map(Some),
    };
    let context_window = positive_integer("contextWindow")?;
    let max_tokens = positive_integer("maxOutputTokens")?;
    let supports_images = msg
        .get("supportsImages")
        .and_then(serde_json::Value::as_bool);
    crate::config::add_provider(
        provider,
        api,
        base_url,
        api_key,
        model_id,
        model_name,
        context_window,
        max_tokens,
        supports_images,
    )?;
    Ok(serde_json::json!({
        "provider": provider,
        "modelId": model_id,
        "api": api,
        "requiresRestart": true,
    }))
}

fn rpc_update_provider_config(msg: &serde_json::Value) -> Result<serde_json::Value, String> {
    let required = |name: &str| {
        msg.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("update_provider_config requires {name}"))
    };
    let positive_integer = |name: &str| match msg.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
            .ok_or_else(|| format!("{name} must be a positive integer"))
            .map(Some),
    };
    let provider = required("provider")?;
    let api = required("api")?;
    let base_url = required("baseUrl")?;
    let model_id = required("modelId")?;
    crate::config::update_provider(
        provider,
        api,
        base_url,
        msg.get("apiKey").and_then(serde_json::Value::as_str),
        model_id,
        msg.get("modelName").and_then(serde_json::Value::as_str),
        positive_integer("contextWindow")?,
        positive_integer("maxOutputTokens")?,
        msg.get("supportsImages")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    )?;
    Ok(serde_json::json!({
        "provider": provider,
        "modelId": model_id,
        "requiresRestart": true,
    }))
}

fn rpc_delete_provider_config(msg: &serde_json::Value) -> Result<serde_json::Value, String> {
    let provider = msg
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "delete_provider_config requires provider".to_string())?;
    crate::config::delete_provider(provider)?;
    Ok(serde_json::json!({
        "provider": provider,
        "requiresRestart": true,
    }))
}

async fn rpc_set_api_key(msg: &serde_json::Value) -> Result<serde_json::Value, String> {
    let provider = msg
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "set_api_key requires a provider".to_string())?;
    let api_key = msg
        .get("apiKey")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "set_api_key requires a non-empty apiKey".to_string())?;
    crate::config::upsert_credential(
        provider,
        crate::config::Credential::ApiKey {
            key: Some(api_key.to_string()),
            env: None,
        },
    )
    .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "provider": provider,
        "ready": true,
        "source": "auth.json",
        "requiresRestart": true,
    }))
}

fn rpc_get_available_models(catalog: &[rpi_ai::Model]) -> serde_json::Value {
    let models = catalog
        .iter()
        .map(crate::rpc::protocol::model_descriptor)
        .collect::<Vec<_>>();
    serde_json::json!({"models": models})
}

async fn rpc_get_available_thinking_levels(lane: &Arc<dyn AgentLane>) -> serde_json::Value {
    let levels = lane
        .get_model()
        .await
        .map(|model| model.supported_thinking_levels())
        .unwrap_or_default();
    serde_json::json!({"levels": levels})
}

async fn rpc_configure_external_tools(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    run_active: bool,
    backend: &Arc<crate::rpc::external_tools::ExternalTools>,
    old_names: &mut Vec<String>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use rpi_harness::types::HarnessTool;
    use rpi_tools::tools::external::{
        create_external_tool, validate_external_tool_definitions, ExternalToolDefinition,
    };
    if run_active
        || lane
            .snapshot()
            .await
            .map_err(|e| e.to_string())?
            .active
            .is_some()
    {
        return Err("cannot configure external tools while a run is active".into());
    }
    let definitions: Vec<ExternalToolDefinition> =
        serde_json::from_value(msg["tools"].clone()).map_err(|e| e.to_string())?;
    let mut registry = harness.get_tools().await.map_err(|e| e.to_string())?;
    registry.retain(|tool| !old_names.contains(&tool.tool.schema().name));
    // Reserve security/native names even when unavailable or disabled at launch.
    let mut reserved = [
        "read",
        "write",
        "edit",
        "bash",
        "powershell",
        "docs",
        "grep",
        "find",
        "ls",
        "image",
        "permissions",
        "permission",
        "ask_user",
        "plan_mode_start",
        "plan_mode_complete",
    ]
    .iter()
    .map(|name| name.to_string())
    .collect::<Vec<_>>();
    reserved.extend(registry.iter().map(|tool| tool.tool.schema().name.clone()));
    validate_external_tool_definitions(&definitions, &reserved).map_err(|e| e.to_string())?;
    let names = definitions
        .iter()
        .map(|def| def.name.clone())
        .collect::<Vec<_>>();
    let mut active = lane.get_active_tools().await.map_err(|e| e.to_string())?;
    active.retain(|name| !old_names.contains(name));
    active.extend(names.clone());
    registry.extend(
        definitions
            .into_iter()
            .map(|def| HarnessTool::new(create_external_tool(def, backend.clone()))),
    );
    harness
        .set_tools(registry, Some(active.clone()))
        .await
        .map_err(|e| e.to_string())?;
    *old_names = names.clone();
    Ok(serde_json::json!({"tools": names, "activeTools": active}))
}

async fn rpc_get_available_tools(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
) -> serde_json::Value {
    let active: Vec<String> = if harness.has_exact_resources() {
        harness
            .execution_context()
            .ok()
            .and_then(|v| serde_json::from_value(v["tools"].clone()).ok())
            .unwrap_or_default()
    } else {
        lane.get_active_tools().await.unwrap_or_default()
    };
    let tools = harness
        .get_tools()
        .await
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool.tool.schema().name.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|_| active.clone());
    serde_json::json!({"tools": tools, "activeTools": active})
}

async fn rpc_set_model(
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = msg
        .get("modelId")
        .or_else(|| msg.get("model"))
        .and_then(|v| v.as_str())
        .ok_or("set_model requires `modelId`")?;
    let provider = msg.get("provider").and_then(|v| v.as_str());
    let model = catalog
        .iter()
        .find(|m| {
            m.id.eq_ignore_ascii_case(id)
                && provider.map_or(true, |provider| m.provider.eq_ignore_ascii_case(provider))
        })
        .cloned()
        .ok_or_else(|| format!("unknown model: {id}"))?;
    lane.set_model(model).await.map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"modelId": id, "provider": provider}))
}

async fn rpc_set_thinking_level(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let value = msg
        .get("level")
        .or_else(|| msg.get("thinkingLevel"))
        .ok_or("set_thinking_level requires `level`")?;
    let level: rpi_ai::types::ThinkingLevel =
        serde_json::from_value(value.clone()).map_err(|_| {
            "invalid thinking level (off|minimal|low|medium|high|xhigh|max)".to_string()
        })?;
    lane.set_thinking_level(level)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"thinkingLevel": level}))
}

async fn rpc_set_active_tools(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let tools = msg
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or("tool names must be strings")
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .ok_or("set_active_tools requires a `tools` array")??;
    lane.set_active_tools(tools.clone())
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"activeTools": tools}))
}

/// Stable JSON projection for the fine-grained agent lifecycle stream.
///
/// Delegates to the shared [`crate::remote::protocol::RemoteEvent`] so the
/// server and the `--connect` client agree on the wire shape by construction.
fn agent_event_json(event: &AgentEvent) -> serde_json::Value {
    crate::rpc::events::project(event)
}

/// Emit a single harness event as a JSON line on stdout. Mirrors the TS
/// `toJsonEvent` projection (here a lossy but stable shape: `type` + the event
/// payload's key fields).
fn emit_json_event(event: &HarnessEvent) {
    let line = match event {
        HarnessEvent::CompactionProgress(e) => {
            serde_json::json!({ "type": "compaction_progress", "lane": e.lane, "manual": e.manual, "active": e.active })
        }
        HarnessEvent::RunStart(e) => serde_json::json!({
            "type": "run_start",
            "lane": e.lane,
            "runId": e.run_id,
        }),
        HarnessEvent::RunEnd(e) => serde_json::json!({
            "type": "run_end",
            "lane": e.lane,
            "runId": e.run_id,
            "outcome": run_end_outcome_str(e.outcome),
            "leafId": e.leaf_id,
        }),
    };
    println!("{line}");
}

fn run_end_outcome_str(o: RunEndOutcome) -> &'static str {
    match o {
        RunEndOutcome::Completed => "completed",
        RunEndOutcome::Aborted => "aborted",
        RunEndOutcome::Failed => "failed",
    }
}

/// `interactive` mode: uses TUI if terminal supports it, falls back to minimal REPL.
///
/// `event_rx` carries the live `AgentEvent` stream (drained by the TUI to
/// render streaming responses). The REPL fallback ignores it.
///
/// `model_catalog` is the resolved provider's full model list, passed through
/// so the TUI's `/model` selector can display available models (read-only —
/// v1 does not switch models mid-session; see the initial port notes (retired)).
pub async fn interactive(
    harness: &AgentHarness,
    event_rx: Option<tokio::sync::broadcast::Receiver<rpi_agent::AgentEvent>>,
    args: &Args,
    model_catalog: Vec<rpi_ai::Model>,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
    theme: Option<&str>,
    no_themes: bool,
    reload_context: &crate::session::ReloadContext,
) -> i32 {
    // ---- Session lifecycle (P1): BeforeTuiStart ----
    // `BeforeTuiStart`: extensions may prepare (connect a server, load a
    // resource) before the interactive UI initializes. Runs with a per-event
    // timeout; a handler returning `EVENT_HANDLER_ABORT` **vetoes** startup —
    // we print the reason and exit without entering the TUI. No-op when no
    // extension subscribes.
    if let Some(reason) = crate::session::dispatch_session_event_async(
        reload_context,
        rpi_plugin_sdk::EventTag::BeforeTuiStart,
    )
    .await
    {
        eprintln!("[rpi] startup vetoed by extension: {reason}");
        return crate::app::EXIT_VETOED;
    }

    // Check if TUI is supported
    let force_tui = std::env::var("RPI_FORCE_TUI")
        .map(|v| v == "1")
        .unwrap_or(false);
    let code = if force_tui || crate::interactive_tui::is_tui_supported() {
        // Use TUI-based interactive mode
        crate::interactive_tui::interactive_tui(
            harness,
            event_rx,
            args,
            model_catalog,
            initial,
            extra_messages,
            initial_images,
            theme,
            no_themes,
            reload_context,
        )
        .await
    } else {
        // Fall back to simple REPL. No transcript container to hold notes, so
        // the startup notices print as plain lines instead.
        for notice in &reload_context.startup_notices {
            eprintln!("note: {notice}");
        }
        interactive_repl(harness, args, initial, extra_messages, initial_images).await
    };

    // `SessionShutdown`: the interactive session closed (TUI/REPL returned) —
    // extensions release resources, disconnect, persist state. Fired after the
    // UI returns so it always runs (even on early-return paths inside the TUI).
    // A veto here is **advisory** (the session is already closing) — warn and
    // continue.
    if let Some(reason) = crate::session::dispatch_session_event_async(
        reload_context,
        rpi_plugin_sdk::EventTag::SessionShutdown,
    )
    .await
    {
        tracing::warn!(
            "[rpi] extension vetoed SessionShutdown (ignored, session closing): {reason}"
        );
    }

    code
}

/// Simple REPL-based interactive mode (fallback for non-TTY environments).
pub async fn interactive_repl(
    harness: &AgentHarness,
    #[allow(unused_variables)] args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
) -> i32 {
    // Debug: confirm we entered REPL mode
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let stdin = std::io::stdin();
    let is_tty = stdin.is_terminal();

    if is_tty {
        println!(
            "rpi interactive (v1 minimal REPL). Type /exit to quit, /abort to cancel a run.\n"
        );
    }

    // Run the initial prompt + extra messages first (same as print mode).
    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }
    let mut images = initial_images;
    for prompt in prompts {
        if let Err(code) = run_one(&lane, &prompt, std::mem::take(&mut images)).await {
            return code;
        }
    }

    // Then read lines from stdin until EOF / `/exit`.
    let mut line = String::new();
    loop {
        if is_tty {
            print!("> ");
            let _ = std::io::stdout().flush();
        }
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }
        if trimmed == "/abort" {
            let _ = lane.abort().await;
            eprintln!("(aborted)");
            continue;
        }
        if let Err(code) = run_one(&lane, trimmed, Vec::new()).await {
            return code;
        }
    }
    0
}

/// Run a single prompt in interactive mode, printing the assistant reply (or
/// the error). Returns `Ok(())` on success/soft-failure, `Err(exit_code)` on a
/// hard rejection.
async fn run_one(
    lane: &Arc<dyn AgentLane>,
    prompt: &str,
    images: Vec<ImageContent>,
) -> Result<(), i32> {
    match lane.prompt_text(prompt, images).await {
        Ok(result) => {
            match &result.outcome {
                HarnessRunOutcome::Completed { final_message, .. }
                | HarnessRunOutcome::Aborted { final_message, .. } => {
                    let text = assistant_text(final_message);
                    if !text.is_empty() {
                        println!("{text}");
                    }
                }
                HarnessRunOutcome::Failed {
                    error,
                    final_message,
                    ..
                } => {
                    if let Some(m) = final_message {
                        if let Some(em) = &m.error_message {
                            eprintln!("error: {em}");
                        }
                    }
                    eprintln!("run failed: {error:?}");
                }
                HarnessRunOutcome::Suspended { .. } => {
                    eprintln!("run suspended (deferred) — resume not supported in v1");
                }
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("prompt rejected: {e}");
            Err(1)
        }
    }
}

/// The print-mode (headless) message for a refused session write.
///
/// Print mode has no transcript to fall back on and no interactive `/export`
/// either — the process is about to exit. Naming a recovery the user cannot run
/// from here would be worse than saying what was lost, so this deliberately does
/// not recommend exporting (unlike the TUI, where `/export` does work).
fn session_storage_notice(message: &str) -> String {
    format!(
        "conversation not saved: {message} — the session log is missing this turn \
         and print mode exits without writing it"
    )
}

#[cfg(test)]
#[path = "rpc/native_dispatch_tests.rs"]
mod native_dispatch_tests;
#[cfg(all(test, feature = "experimental-native-store"))]
#[path = "modes/native_persistence_tests.rs"]
mod native_persistence_tests;
#[cfg(test)]
#[path = "modes/transcript_page_tests.rs"]
mod transcript_page_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::events::AgentEvent;
    use rpi_agent::types::AgentToolResult;
    use rpi_ai::types::{
        AssistantMessage, Content, StopReason, TextContent, TextContentType, Usage,
    };
    use rpi_harness::session::types::OperationError;

    /// Regression: the abort reply must carry the reclaimed queue back instead
    /// of only `{aborted:true}`, and each queue must be reported separately.
    #[test]
    fn abort_response_hands_back_the_reclaimed_queues() {
        let user = |text: &str| {
            rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text(text.to_string()),
                0,
            ))
        };
        let result = rpi_harness::agent_harness::AbortResult {
            run_id: "run-1".to_string(),
            steer: vec![user("S")],
            follow_up: vec![user("F")],
        };

        let reply = abort_response(&result);
        assert_eq!(reply["aborted"], serde_json::json!(true));
        assert_eq!(reply["steer"][0]["text"], serde_json::json!("S"));
        assert_eq!(reply["followUp"][0]["text"], serde_json::json!("F"));
        // Steering must not absorb the follow-up entry.
        assert_eq!(reply["steer"].as_array().unwrap().len(), 1);

        // The wrapper-cancellation path reports the same shape.
        assert_eq!(
            reclaimed_payload(&result),
            serde_json::json!({
                "steer": reply["steer"].clone(),
                "followUp": reply["followUp"].clone(),
            })
        );
    }

    include!("rpc/external_tools_configuration_tests.rs");
    include!("rpc/external_tools_lifecycle_tests.rs");
    include!("rpc/external_tools_abort_outcome_tests.rs");

    #[test]
    fn session_branch_query_requires_canonical_arguments() {
        let query = branch_entry_query(&serde_json::json!({
            "customTypes": ["alpha", "beta", "alpha"], "afterSeq": 42,
        }))
        .unwrap();
        assert_eq!(query.custom_types, ["alpha", "beta"]);
        assert_eq!(query.order, EntryOrder::OldestFirst);
        assert_eq!(query.after_seq, Some(42));
        assert!(query.entry_queries().iter().all(|q| q.limit.is_none()));
        for invalid in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({"customTypes": []}),
            serde_json::json!({"customTypes": "alpha"}),
            serde_json::json!({"customTypes": [""]}),
            serde_json::json!({"customTypes": [" \t "]}),
            serde_json::json!({"customTypes": ["alpha", 1]}),
            serde_json::json!({"customTypes": ["alpha"], "limit": 0}),
            serde_json::json!({"customTypes": ["alpha"], "limit": 1.5}),
            serde_json::json!({"customTypes": ["alpha"], "order": "sideways"}),
            serde_json::json!({"customTypes": ["alpha"], "order": 1}),
            serde_json::json!({"customTypes": ["alpha"], "afterSeq": -1}),
        ] {
            assert!(branch_entry_query(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn session_branch_multi_type_limit_is_pushed_to_each_storage_query() {
        for order in ["oldestFirst", "newestFirst"] {
            let request = branch_entry_query(&serde_json::json!({
                "customTypes": ["alpha", "beta", "alpha"],
                "limit": 2, "order": order, "afterSeq": 42,
            }))
            .unwrap();
            let queries = request.entry_queries();
            assert_eq!(queries.len(), 2);
            for query in queries {
                assert_eq!(query.limit, Some(2));
                assert_eq!(query.order, Some(request.order));
                assert_eq!(query.cursor, Some(EntryCursor { after_seq: 42 }));
            }
        }
    }

    #[tokio::test]
    async fn resource_selection_binding_is_protected_for_scoped_and_unscoped_public_appends() {
        use rpi_extensions::RuntimeActionHost;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (host, cell) = crate::extensions_actions::HarnessActionHost::new_empty(
            vec![],
            std::env::temp_dir(),
            tokio::runtime::Handle::current(),
            Default::default(),
            rpi_extensions::ToolCallContext::default(),
        );
        crate::extensions_actions::HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
        let (_, session_id, leaf_id) = branch_snapshot(
            &harness,
            &branch_entry_query(&serde_json::json!({"customTypes":["rpi.value"]})).unwrap(),
        )
        .await
        .unwrap();
        let guard = serde_json::json!({
            "sessionId": session_id, "leafId": leaf_id,
            "customType": "rpi.value", "entryId": null
        });
        for key in ["rpi.resource-selection.v1", "rpi.resource-inventory.v1"] {
            for value in [serde_json::Value::Null, serde_json::json!({"forged":true})] {
                for scoped in [false, true] {
                    let mut request = serde_json::json!({
                        "customType":"rpi.value", "data":{"key":key,"value":value}
                    });
                    if scoped {
                        request["expected"] = guard.clone();
                    }
                    assert!(rpc_append_entry(&harness, &request)
                        .await
                        .unwrap_err()
                        .contains("host-owned"));
                    assert!(host
                        .append_entry(request)
                        .await
                        .unwrap_err()
                        .contains("host-owned"));
                }
            }
        }
        let request = serde_json::json!({
            "customType":"rpi.value", "data":{"key":"user.preference","value":1},
            "expected":guard
        });
        assert_eq!(host.append_entry(request).await.unwrap()["scoped"], true);
    }

    #[tokio::test]
    async fn branch_snapshot_guards_and_outbox_acknowledgements_cross_the_runtime_action_boundary()
    {
        use rpi_extensions::RuntimeActionHost;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (host, cell) = crate::extensions_actions::HarnessActionHost::new_empty(
            vec![],
            std::env::temp_dir(),
            tokio::runtime::Handle::current(),
            Default::default(),
            rpi_extensions::ToolCallContext::default(),
        );
        crate::extensions_actions::HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
        // Preview's CAS API must not bypass main's host-owned binding guard.
        let before = harness
            .session()
            .find_entries(&EntryQuery::default())
            .await
            .unwrap();
        let values = host
            .get_session_branch(serde_json::json!({"customTypes":["rpi.value"]}))
            .await
            .unwrap();
        for key in ["rpi.resource-selection.v1", "rpi.resource-inventory.v1"] {
            let forged = serde_json::json!({
                "customType":"rpi.value", "data":{"key":key,"value":{}},
                "expected":{"sessionId":values["sessionId"],"leafId":values["leafId"],"customType":"rpi.value","entryId":null}
            });
            assert!(host
                .append_entry(forged.clone())
                .await
                .unwrap_err()
                .contains("host-owned"));
            assert!(rpc_append_entry(&harness, &forged)
                .await
                .unwrap_err()
                .contains("host-owned"));
        }
        assert_eq!(
            harness
                .session()
                .find_entries(&EntryQuery::default())
                .await
                .unwrap(),
            before
        );
        let snapshot = host
            .get_session_branch(
                serde_json::json!({"customTypes":["goal-state"],"limit":1,"order":"newestFirst"}),
            )
            .await
            .unwrap();
        let expected = serde_json::json!({"sessionId":snapshot["sessionId"],"leafId":snapshot["leafId"],"customType":"goal-state","entryId":null});
        let mutation =
            serde_json::json!({"customType":"goal-state","data":{"goal":"A"},"expected":expected});
        let ack = host.append_entry(mutation.clone()).await.unwrap();
        assert_eq!(ack["scoped"], true);
        assert!(host.append_entry(mutation.clone()).await.is_err());
        assert!(rpc_append_entry(&harness, &mutation).await.is_err());
        for _ in 0..2 {
            let snapshot = host.get_session_branch(serde_json::json!({"customTypes":["goal-state"],"limit":1,"order":"newestFirst"})).await.unwrap();
            let expected = serde_json::json!({"sessionId":snapshot["sessionId"],"leafId":snapshot["leafId"],"customType":"goal-state","entryId":ack["entryId"]});
            let notice = host.append_entry(serde_json::json!({"expected":expected,"outboxKey":"r1","message":{
                "kind":"custom","role":"custom","content":[{"type":"text","text":"Goal A"}],"data":{"customType":"goal"},"timestamp":1
            }})).await.unwrap();
            assert_eq!(notice["scoped"], true);
        }
        let entries = harness
            .session()
            .find_entries(&EntryQuery {
                outbox_key: Some("r1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        let snapshot = host
            .get_session_branch(serde_json::json!({"customTypes":["goal-state"],"limit":1}))
            .await
            .unwrap();
        let expected = serde_json::json!({"sessionId":snapshot["sessionId"],"leafId":snapshot["leafId"],"customType":"goal-state","entryId":ack["entryId"]});
        let rpc_ack = rpc_append_entry(
            &harness,
            &serde_json::json!({"customType":"goal-state","data":{"goal":"B"},"expected":expected}),
        )
        .await
        .unwrap();
        assert_eq!(rpc_ack["scoped"], true);
        assert!(rpc_ack["entryId"].as_str().is_some_and(|id| !id.is_empty()));
    }

    #[tokio::test]
    async fn session_branch_exact_type_identities_round_trip_rpc_and_actions() {
        use rpi_extensions::RuntimeActionHost;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (host, cell) = crate::extensions_actions::HarnessActionHost::new_empty(
            vec![],
            std::env::temp_dir(),
            tokio::runtime::Handle::current(),
            Default::default(),
            rpi_extensions::ToolCallContext::default(),
        );
        crate::extensions_actions::HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
        let names = ["alpha", " alpha ", "beta", " beta "];
        for (index, name) in names.iter().enumerate() {
            let args = serde_json::json!({"customType": name, "data": {"n": index}});
            if index % 2 == 0 {
                host.append_entry(args).await.unwrap();
            } else {
                rpc_append_entry(&harness, &args).await.unwrap();
            }
        }
        let request = branch_entry_query(&serde_json::json!({
            "customTypes": ["alpha", " alpha ", "beta", " beta ", "alpha"],
        }))
        .unwrap();
        assert_eq!(request.custom_types, names);
        let mut input = String::new();
        let mut expected = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let args = serde_json::json!({"customTypes": [name]});
            let result = host.get_session_branch(args.clone()).await.unwrap();
            assert_eq!(result["count"], 1);
            assert_eq!(result["entries"][0]["customType"], *name);
            assert_eq!(result["entries"][0]["data"]["n"], index);
            let mut envelope = args;
            envelope["type"] = serde_json::json!("get_session_branch");
            envelope["id"] = serde_json::json!(index);
            input.push_str(&format!("{envelope}\n"));
            expected.push(result);
        }
        let output = RpcOutput::default();
        assert_eq!(
            rpc_with_io(
                &harness,
                None,
                vec![],
                rpi_extensions::UiDialogMailbox::new(),
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input.as_bytes()),
                output.clone(),
            )
            .await,
            0
        );
        let messages = output.messages();
        for (index, expected) in expected.into_iter().enumerate() {
            let response = messages
                .iter()
                .find(|message| message["id"] == index)
                .unwrap();
            assert_eq!(response["data"], expected);
        }
    }

    #[tokio::test]
    async fn session_branch_filters_merge_and_respect_fork_bounds() {
        use rpi_harness::session::types::{ForkOptions, ForkPosition, SessionRepo};
        use rpi_harness::session::{CounterIdGenerator, FakeClock, InMemorySessionRepo, Session};
        let repo = InMemorySessionRepo::new(
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        );
        let storage = repo.create(&Default::default()).await.unwrap();
        let session = Session::new(Arc::new(storage), None);
        let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
            session,
            ..Default::default()
        })
        .await
        .unwrap();
        let session = harness.session();
        let empty = rpc_get_session_branch(
            &harness,
            &serde_json::json!({"customTypes":["alpha","beta"]}),
        )
        .await
        .unwrap();
        assert_eq!(empty["entries"], serde_json::json!([]));
        assert_eq!(empty["filtered"], true);
        let mut ids = Vec::new();
        let mut fork_target = None;
        for (index, kind) in ["alpha", "noise", "beta", "alpha", "noise", "beta"]
            .into_iter()
            .enumerate()
        {
            if index == 2 {
                fork_target = Some(
                    session
                        .append_message(rpi_agent::AgentMessage::User(UserMessage::new(
                            "fork target",
                            0,
                        )))
                        .await
                        .unwrap(),
                );
            }
            ids.push(session.append_custom_entry(kind, None).await.unwrap());
        }
        // Neither ordinary messages nor durable lane records are custom entries.
        session
            .append_message(rpi_agent::AgentMessage::User(UserMessage::new("noise", 0)))
            .await
            .unwrap();
        session
            .append_record(
                serde_json::from_value(serde_json::json!({
                    "type":"usage", "id":"usage-noise", "seq":0, "timestamp":0,
                    "lane":"main", "usage":Usage::zero(), "cause":"adjustment"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let run = |args| {
            let harness = &harness;
            async move { rpc_get_session_branch(harness, &args).await.unwrap() }
        };
        let all = run(serde_json::json!({"customTypes":["alpha","beta","alpha"]})).await;
        let entry_ids = |value: &serde_json::Value| -> Vec<String> {
            value["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["id"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            entry_ids(&all),
            vec![
                ids[0].clone(),
                ids[2].clone(),
                ids[3].clone(),
                ids[5].clone()
            ]
        );
        assert_eq!(all["count"], 4);
        assert_eq!(all["filtered"], true);
        let cursor = all["entries"][1]["seq"].as_u64().unwrap();
        for (order, expected) in [
            (
                "oldestFirst",
                vec![ids[0].clone(), ids[2].clone(), ids[3].clone()],
            ),
            (
                "newestFirst",
                vec![ids[5].clone(), ids[3].clone(), ids[2].clone()],
            ),
        ] {
            let result =
                run(serde_json::json!({"customTypes":["alpha","beta"], "order":order, "limit":3}))
                    .await;
            assert_eq!(entry_ids(&result), expected);
            let result = run(serde_json::json!({"customTypes":["alpha","beta"], "order":order, "afterSeq":cursor, "limit":1})).await;
            assert_eq!(
                entry_ids(&result),
                vec![if order == "oldestFirst" {
                    ids[3].clone()
                } else {
                    ids[0].clone()
                }]
            );
        }
        let single = run(serde_json::json!({"customTypes":["beta"], "afterSeq":all["entries"][3]["seq"], "order":"newestFirst", "limit":1})).await;
        assert_eq!(entry_ids(&single), vec![ids[2].clone()]);

        // Rewind and append: old descendants and other lanes must not leak in.
        session.move_lane("main", Some(&ids[2])).await.unwrap();
        let branch_tip = session.append_custom_entry("alpha", None).await.unwrap();
        session.create_lane("other", Some(&ids[5])).await.unwrap();
        let branch = run(serde_json::json!({"customTypes":["alpha","beta"]})).await;
        assert_eq!(
            entry_ids(&branch),
            vec![ids[0].clone(), ids[2].clone(), branch_tip]
        );
        let empty =
            run(serde_json::json!({"customTypes":["alpha","beta"], "afterSeq":u64::MAX - 1})).await;
        assert!(entry_ids(&empty).is_empty());

        // Fork before the message preceding beta; custom entries remain covered.
        let fork = repo
            .fork(
                &session.storage().metadata(),
                &Default::default(),
                &ForkOptions::Branch {
                    entry_id: fork_target,
                    position: Some(ForkPosition::Before),
                },
            )
            .await
            .unwrap();
        harness
            .set_session(Session::new(Arc::new(fork), None))
            .await
            .unwrap();
        let forked = run(
            serde_json::json!({"customTypes":["alpha","beta"], "order":"newestFirst", "limit":2}),
        )
        .await;
        assert_eq!(entry_ids(&forked), vec![ids[0].clone()]);
    }

    #[tokio::test]
    async fn session_branch_rpc_and_action_share_strict_query_behavior() {
        use rpi_extensions::RuntimeActionHost;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (host, cell) = crate::extensions_actions::HarnessActionHost::new_empty(
            vec![],
            std::env::temp_dir(),
            tokio::runtime::Handle::current(),
            Default::default(),
            rpi_extensions::ToolCallContext::default(),
        );
        crate::extensions_actions::HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
        // AppendEntry deliberately keeps its distinct singular write protocol.
        host.append_entry(serde_json::json!({"customType":"alpha", "data":{"n":1}}))
            .await
            .unwrap();
        rpc_append_entry(
            &harness,
            &serde_json::json!({"customType":"beta", "data":{"n":2}}),
        )
        .await
        .unwrap();
        let valid =
            serde_json::json!({"customTypes":["alpha","beta"], "limit":1, "order":"newestFirst"});
        let expected = host.get_session_branch(valid.clone()).await.unwrap();
        assert_eq!(expected["entries"][0]["customType"], "beta");
        let mut requests = vec![serde_json::json!({})];
        for key in [
            "customType",
            "custom_type",
            "custom_types",
            "after_seq",
            "unknown",
        ] {
            let value = if key == "after_seq" {
                serde_json::json!(0)
            } else {
                serde_json::json!("alpha")
            };
            requests.push(serde_json::json!({key:value.clone()}));
            let mut mixed = valid.clone();
            mixed[key] = value;
            requests.push(mixed);
        }
        let mut input = String::new();
        let mut envelope = valid;
        envelope["type"] = serde_json::json!("get_session_branch");
        envelope["id"] = serde_json::json!("modern");
        input.push_str(&format!("{envelope}\n"));
        for (index, args) in requests.iter().enumerate() {
            let error = host.get_session_branch(args.clone()).await.unwrap_err();
            assert!(error.contains("get_session_branch"), "{error}");
            let mut envelope = args.clone();
            envelope["type"] = serde_json::json!("get_session_branch");
            envelope["id"] = serde_json::json!(index);
            assert_eq!(
                rpc_get_session_branch(&harness, &envelope)
                    .await
                    .unwrap_err(),
                error
            );
            input.push_str(&format!("{envelope}\n"));
        }
        let output = RpcOutput::default();
        assert_eq!(
            rpc_with_io(
                &harness,
                None,
                vec![],
                rpi_extensions::UiDialogMailbox::new(),
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input.as_bytes()),
                output.clone(),
            )
            .await,
            0
        );
        let messages = output.messages();
        assert_eq!(
            messages.iter().find(|m| m["id"] == "modern").unwrap()["data"],
            expected
        );
        for index in 0..requests.len() {
            let response = messages.iter().find(|m| m["id"] == index).unwrap();
            assert_eq!(response["success"], false, "{response}");
        }
    }

    #[tokio::test]
    async fn rpc_commands_list_and_execute_registered_handler() {
        extern "C" fn echo(
            input: rpi_plugin_sdk::StbStringRef,
            out: *mut rpi_plugin_sdk::StbString,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            // SAFETY: the RPC invoker provides live input and output pointers.
            unsafe {
                *out = rpi_plugin_sdk::StbString::from_string(input.as_str().to_owned());
            }
            0
        }
        let mut registry = rpi_extensions::ExtensionRegistry::new();
        registry.register_command(
            "echo".into(),
            "Echo arguments".into(),
            echo,
            std::ptr::null_mut(),
        );
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let output = RpcOutput::default();
        let input = concat!(
            "{\"id\":1,\"type\":\"get_commands\"}\n",
            "{\"id\":2,\"type\":\"execute_command\",\"command\":\"/echo\",\"args\":\"hello world\"}\n",
            "{\"id\":3,\"type\":\"execute_command\",\"command\":\"missing\"}\n",
            "{\"id\":4,\"type\":\"execute_command\",\"command\":\"echo\",\"args\":42}\n"
        );
        assert_eq!(
            rpc_with_io(
                &harness,
                None,
                vec![],
                rpi_extensions::UiDialogMailbox::new(),
                Some(Arc::new(registry.snapshot())),
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input.as_bytes()),
                output.clone()
            )
            .await,
            0
        );
        let messages = output.messages();
        let response = |id| messages.iter().find(|m| m["id"] == id).unwrap();
        assert_eq!(response(1)["data"]["commands"][0]["name"], "echo");
        assert_eq!(
            response(2)["data"],
            serde_json::json!({"command":"/echo", "args":"hello world"})
        );
        assert_eq!(response(3)["success"], false);
        assert_eq!(response(4)["success"], false);
        assert_eq!(rpc_get_commands(None), serde_json::json!({"commands":[]}));
    }

    #[test]
    fn rpc_auth_status_shares_header_readiness_and_expired_token_state() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        struct RestoreConfig(Option<std::ffi::OsString>);
        impl Drop for RestoreConfig {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }
        let _restore = RestoreConfig(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let directory = tempfile::tempdir().unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, directory.path());
        std::fs::write(directory.path().join("models.json"), serde_json::json!({"providers": {
            "header-gateway": {"api":"openai-completions", "headers":{"Authorization":"Bearer header-secret"}, "models":[{"id":"model"}]},
            "expired-account": {"api":"openai-completions", "models":[]}
        }}).to_string()).unwrap();
        std::fs::write(directory.path().join("auth.json"), serde_json::json!({
            "expired-account": {"type":"oauth", "access":"expired-secret", "refresh":"refresh-secret", "expires":1}
        }).to_string()).unwrap();
        let status = rpc_get_auth_status(&[], None);
        let providers = status["providers"].as_array().unwrap();
        let header = providers
            .iter()
            .find(|row| row["provider"] == "header-gateway")
            .unwrap();
        assert_eq!(header["ready"], true);
        assert_eq!(header["source"], "headers");
        assert_eq!(header["status"], "ready");
        let expired = providers
            .iter()
            .find(|row| row["provider"] == "expired-account")
            .unwrap();
        assert_eq!(expired["ready"], false);
        assert_eq!(expired["status"], "expired");
        assert!(!status.to_string().contains("secret"));
    }

    #[test]
    fn oauth_display_drops_token_like_fields() {
        let display = oauth_display(&serde_json::json!({
            "display": {
                "title": "Sign in",
                "url": "https://example.test/authorize",
                "instructions": ["Paste the code"],
                "access_token": "secret-access",
                "refresh_token": "secret-refresh",
            }
        }));
        assert_eq!(display["title"], "Sign in");
        assert!(display.get("access_token").is_none());
        assert!(display.get("refresh_token").is_none());
    }

    #[test]
    fn rpc_message_text_joins_content_blocks_with_real_newlines() {
        let text = rpc_message_text(&serde_json::json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ]
        }));
        assert_eq!(text, "first\nsecond");
        assert!(!text.contains("\\n"));
    }

    #[derive(Clone, Default)]
    pub(super) struct RpcOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for RpcOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl RpcOutput {
        pub(super) fn messages(&self) -> Vec<serde_json::Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        pub(super) async fn wait_for(
            &self,
            predicate: impl Fn(&serde_json::Value) -> bool,
        ) -> serde_json::Value {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if let Some(message) = self.messages().into_iter().find(&predicate) {
                        return message;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("RPC message not received")
        }
    }

    pub(super) async fn rpc_test_harness(
        text: &str,
        tokens_per_second: f64,
    ) -> (AgentHarness, Arc<rpi_ai::providers::faux::FauxProvider>) {
        let provider = rpi_ai::providers::faux::FauxProvider::new(
            rpi_ai::providers::faux::FauxScript::new()
                .with_text(text)
                .with_tokens_per_second(tokens_per_second),
        );
        let options = rpi_harness::types::AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            ..Default::default()
        };
        (AgentHarness::create(options).await.unwrap(), provider)
    }

    pub(super) fn start_test_rpc(
        harness: AgentHarness,
        ui: rpi_extensions::UiDialogMailbox,
    ) -> (
        tokio::io::DuplexStream,
        RpcOutput,
        tokio::task::JoinHandle<i32>,
    ) {
        let (client, input) = tokio::io::duplex(4096);
        let output = RpcOutput::default();
        let captured = output.clone();
        let task = tokio::spawn(async move {
            rpc_with_io(
                &harness,
                None,
                vec![],
                ui,
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input),
                captured,
            )
            .await
        });
        (client, output, task)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_paginated_user_summaries_and_context_windows_are_read_only() {
        use tokio::io::AsyncWriteExt;
        let (harness, _unused_provider) = rpc_test_harness("must not be called", 0.0).await;
        let view = harness.view();
        for index in 0..23 {
            view.append_message(
                rpi_ai::UserMessage::new(format!("Question {index}"), index).into(),
            )
            .await
            .unwrap();
        }
        let pinned = view.get_leaf_id().await.unwrap();
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        let ready = output.wait_for(|frame| frame["type"] == "ready").await;
        for command in ["get_message_summaries", "get_message_window"] {
            assert!(ready["supportedCommands"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(command)));
        }
        client
            .write_all(b"{\"type\":\"get_message_summaries\",\"id\":1,\"limit\":10}\n")
            .await
            .unwrap();
        let first = output.wait_for(|frame| frame["id"] == 1).await;
        assert_eq!(first["success"], true);
        assert_eq!(first["data"]["messages"][0]["preview"], "Question 13");
        assert_eq!(first["data"]["messages"].as_array().unwrap().len(), 10);
        for (id, command, cursor, value) in [
            (
                2,
                "get_message_summaries",
                "beforeId",
                first["data"]["nextBeforeId"].clone(),
            ),
            (
                3,
                "get_message_window",
                "anchorId",
                first["data"]["firstId"].clone(),
            ),
        ] {
            let mut request = serde_json::json!({"id":id,"type":command,"limit":4,"leafId":pinned});
            request[cursor] = value;
            client
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let page = output.wait_for(|frame| frame["id"] == id).await;
            assert_eq!(page["success"], true);
            assert_eq!(page["data"]["messages"].as_array().unwrap().len(), 4);
            assert_eq!(page["data"]["leafId"], serde_json::json!(pinned));
        }
        let context = output.wait_for(|frame| frame["id"] == 3).await;
        for (id, cursor, key) in [
            (4, "beforeId", "nextBeforeId"),
            (5, "afterId", "nextAfterId"),
        ] {
            let mut request =
                serde_json::json!({"type":"get_message_window", "id":id,"leafId":pinned,"limit":4});
            request[cursor] = context["data"][key].clone();
            client
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let response = output.wait_for(|frame| frame["id"] == id).await;
            assert_eq!(response["success"], true);
            assert_eq!(response["data"]["messages"].as_array().unwrap().len(), 4);
        }
        client
            .write_all(b"{\"type\":\"stop\",\"id\":6}\n")
            .await
            .unwrap();
        assert_eq!(task.await.unwrap(), 0);
        assert_eq!(view.get_leaf_id().await.unwrap(), pinned);
        assert!(!output
            .messages()
            .iter()
            .any(|frame| frame["type"] == "agent_start"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_eof_waits_for_accepted_prompt() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("completed", 100.0).await;
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let response = output
            .messages()
            .into_iter()
            .find(|m| m["id"] == 1)
            .unwrap();
        assert_eq!(response["command"], "prompt");
        assert_eq!(response["data"]["accepted"], true);
        assert!(output.messages().iter().any(|message| {
            message["type"] == "agent_settled" && message["outcome"] == "completed"
        }));
        assert!(!ui.is_attached());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_append_entry_publishes_generic_commit_event() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("unused", 0.0).await;
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client
            .write_all(
                br#"{"type":"append_entry","id":1,"customType":"durable-state","data":{"revision":1}}
"#,
            )
            .await
            .unwrap();

        let event = output
            .wait_for(|message| message["type"] == "session_entry_committed")
            .await;
        assert_eq!(event["customType"], "durable-state");
        assert_eq!(event["entryType"], "custom");
        assert_eq!(event["lane"], "main");
        assert!(event["entryId"].as_str().is_some());

        client
            .write_all(
                br#"{"type":"stop","id":2}
"#,
            )
            .await
            .unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_prompt_publishes_committed_message_event() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("completed", 0.0).await;
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
            .await
            .unwrap();
        let event = output
            .wait_for(|message| {
                message["type"] == "session_entry_committed" && message["entryType"] == "message"
            })
            .await;
        assert_eq!(event["lane"], "main");
        assert!(event["entryId"].as_str().is_some());
        output
            .wait_for(|message| message["type"] == "agent_settled")
            .await;
        client
            .write_all(b"{\"type\":\"stop\",\"id\":2}\n")
            .await
            .unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn rpc_stop_flushes_all_accepted_entry_invalidations() {
        let (harness, _) = rpc_test_harness("unused", 0.0).await;
        let output = RpcOutput::default();
        let mut input = String::new();
        for id in 1..=4 {
            input.push_str(&format!(
                "{{\"type\":\"append_entry\",\"id\":{id},\"customType\":\"durable-state\"}}\n"
            ));
        }
        input.push_str("{\"type\":\"stop\",\"id\":5}\n");
        let result = rpc_with_io(
            &harness,
            None,
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(input.as_bytes()),
            output.clone(),
        )
        .await;
        assert_eq!(result, 0);
        let messages = output.messages();
        let commits: std::collections::HashSet<_> = messages
            .iter()
            .filter(|m| m["type"] == "session_entry_committed")
            .map(|m| m["entryId"].as_str().unwrap())
            .collect();
        assert_eq!(
            commits.len(),
            4,
            "stop must flush every committed entry: {messages:?}"
        );
    }

    #[tokio::test]
    async fn rpc_eof_flushes_queued_entry_invalidations() {
        let (harness, _) = rpc_test_harness("unused", 0.0).await;
        let output = RpcOutput::default();
        let input = b"{\"type\":\"append_entry\",\"id\":1,\"customType\":\"durable-state\",\"data\":{\"revision\":1}}\n";
        let result = rpc_with_io(
            &harness,
            None,
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(&input[..]),
            output.clone(),
        )
        .await;
        assert_eq!(result, 0);
        let messages = output.messages();
        assert!(
            messages
                .iter()
                .any(|m| m["type"] == "response" && m["id"] == 1 && m["success"] == true),
            "append succeeded: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m["type"] == "session_entry_committed"),
            "EOF must flush the queued commit event: {messages:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_compaction_settlement_identifies_its_durable_operation() {
        let (harness, _) = rpc_test_harness("unused", 0.0).await;
        let output = RpcOutput::default();
        let code = rpc_with_io(
            &harness,
            None,
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(&b"{\"type\":\"compact\",\"id\":1}\n"[..]),
            output.clone(),
        )
        .await;
        assert_eq!(code, 0);
        let messages = output.messages();
        let settled = messages
            .iter()
            .find(|message| message["type"] == "agent_settled")
            .unwrap();
        assert_eq!(settled["outcome"], "declined");
        let run_id = settled["runId"]
            .as_str()
            .expect("compaction settlement needs its durable run id");
        let records = harness
            .session()
            .find_records(&Default::default())
            .await
            .unwrap();
        assert!(records.iter().any(|record| matches!(record,
            rpi_harness::session::types::LaneRecord::OperationFinished(finished)
                if finished.run_id == run_id)));
    }

    #[tokio::test]
    async fn rpc_eof_drains_queued_final_events() {
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        for _ in 0..100 {
            tx.send(AgentEvent::AgentStart).unwrap();
        }
        tx.send(AgentEvent::AgentEnd { messages: vec![] }).unwrap();
        let output = RpcOutput::default();
        let code = rpc_with_io(
            &harness,
            Some(rx),
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(&b""[..]),
            output.clone(),
        )
        .await;
        assert_eq!(code, 0);
        let messages = output.messages();
        assert_eq!(
            messages.len(),
            102,
            "ready plus every queued event must be flushed"
        );
        assert_eq!(messages.last().unwrap()["type"], "agent_end");
    }

    fn assert_rpc_update_mode(
        messages: &[serde_json::Value],
        mode: crate::args::RpcMessageUpdates,
    ) {
        let updates: Vec<_> = messages
            .iter()
            .filter(|m| {
                matches!(
                    m["eventType"].as_str(),
                    Some("text_delta" | "thinking_delta" | "toolcall_delta")
                )
            })
            .collect();
        assert!(
            !updates.is_empty(),
            "expected streaming deltas: {messages:?}"
        );
        for update in updates {
            let snapshot = mode == crate::args::RpcMessageUpdates::Snapshot;
            assert_eq!(update.get("message").is_some(), snapshot, "{update}");
            assert_eq!(
                update["assistantMessageEvent"].get("partial").is_some(),
                snapshot,
                "{update}"
            );
            assert_eq!(update["eventType"], update["assistantMessageEvent"]["type"]);
            assert_eq!(
                update["contentIndex"],
                update["assistantMessageEvent"]["contentIndex"]
            );
            assert_eq!(update["delta"], update["assistantMessageEvent"]["delta"]);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_selected_updates_survive_prompt_eof_and_stop() {
        use tokio::io::AsyncWriteExt;
        for mode in [
            crate::args::RpcMessageUpdates::CompactDeltas,
            crate::args::RpcMessageUpdates::Snapshot,
        ] {
            for stop in [false, true] {
                let (harness, _) =
                    rpc_test_harness("one two three four five six seven eight", 100.0).await;
                let (emitter, events) = rpi_agent::events::BroadcastEmitter::new(256);
                harness
                    .set_agent_emitter(Some(Arc::new(emitter)))
                    .await
                    .unwrap();
                let (mut client, input) = tokio::io::duplex(4096);
                let output = RpcOutput::default();
                let captured = output.clone();
                let task = tokio::spawn(async move {
                    rpc_with_io(
                        &harness,
                        Some(events),
                        vec![],
                        rpi_extensions::UiDialogMailbox::new(),
                        None,
                        mode,
                        tokio::io::BufReader::new(input),
                        captured,
                    )
                    .await
                });
                let ready = output.wait_for(|m| m["type"] == "ready").await;
                assert_eq!(ready["semantics"]["messageUpdates"], mode.as_str());
                client
                    .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
                    .await
                    .unwrap();
                if stop {
                    output.wait_for(|m| m["eventType"] == "text_delta").await;
                    client
                        .write_all(b"{\"type\":\"stop\",\"id\":2}\n")
                        .await
                        .unwrap();
                } else {
                    client.shutdown().await.unwrap();
                }
                assert_eq!(
                    tokio::time::timeout(std::time::Duration::from_secs(3), task)
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
                let messages = output.messages();
                assert_rpc_update_mode(&messages, mode);
                assert!(messages.iter().any(|m| m["type"] == "message_end"));
                assert!(messages.iter().any(|m| m["type"] == "agent_end"));
                assert!(messages.iter().any(|m| m["type"] == "agent_settled"));
                if !stop {
                    assert!(messages
                        .iter()
                        .any(|m| m["type"] == "agent_settled" && m["outcome"] == "completed"));
                }
            }
        }
    }

    #[tokio::test]
    async fn rpc_normal_and_explicit_drain_use_the_same_immutable_writer_mode() {
        for mode in [
            crate::args::RpcMessageUpdates::CompactDeltas,
            crate::args::RpcMessageUpdates::Snapshot,
        ] {
            let output = RpcOutput::default();
            let (writer, task, _) = start_rpc_writer(output.clone(), mode);
            let cloned_writer = writer.clone();
            let mut message = rpi_ai::AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "m", 42);
            message.content = vec![rpi_ai::Content::text("snapshot")];
            let message = Arc::new(message);
            let (tx, mut rx) = tokio::sync::broadcast::channel(16);
            for kind in 0..3 {
                let assistant_message_event = match kind {
                    0 => rpi_ai::AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: "text".into(),
                        partial: message.clone(),
                    },
                    1 => rpi_ai::AssistantMessageEvent::ThinkingDelta {
                        content_index: 1,
                        delta: "thought".into(),
                        partial: message.clone(),
                    },
                    _ => rpi_ai::AssistantMessageEvent::ToolCallDelta {
                        content_index: 2,
                        delta: "{}".into(),
                        partial: message.clone(),
                    },
                };
                let event = AgentEvent::MessageUpdate {
                    message_id: None,
                    message: message.clone(),
                    assistant_message_event,
                };
                write_agent_event(&writer, &event).await;
                tx.send(event).unwrap();
            }
            drain_rpc_events(&cloned_writer, &mut rx).await;
            drop(cloned_writer);
            drop(writer);
            task.await.unwrap().unwrap();
            let messages = output.messages();
            assert_eq!(messages.len(), 6);
            assert_eq!(messages[..3], messages[3..]);
            assert_rpc_update_mode(&messages, mode);
        }
    }

    #[tokio::test]
    async fn rpc_queued_deltas_are_flushed_on_eof_and_stop_in_selected_mode() {
        for mode in [
            crate::args::RpcMessageUpdates::CompactDeltas,
            crate::args::RpcMessageUpdates::Snapshot,
        ] {
            for input in ["", "{\"type\":\"stop\",\"id\":1}\n"] {
                let harness = AgentHarness::create(Default::default()).await.unwrap();
                let (tx, rx) = tokio::sync::broadcast::channel(256);
                let mut message =
                    rpi_ai::AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "m", 42);
                message.content = vec![rpi_ai::Content::text("snapshot")];
                let message = Arc::new(message);
                for _ in 0..100 {
                    tx.send(AgentEvent::MessageUpdate {
                        message_id: None,
                        message: message.clone(),
                        assistant_message_event: rpi_ai::AssistantMessageEvent::TextDelta {
                            content_index: 0,
                            delta: "x".into(),
                            partial: message.clone(),
                        },
                    })
                    .unwrap();
                }
                tx.send(AgentEvent::AgentEnd { messages: vec![] }).unwrap();
                let output = RpcOutput::default();
                assert_eq!(
                    rpc_with_io(
                        &harness,
                        Some(rx),
                        vec![],
                        rpi_extensions::UiDialogMailbox::new(),
                        None,
                        mode,
                        tokio::io::BufReader::new(input.as_bytes()),
                        output.clone()
                    )
                    .await,
                    0
                );
                let messages = output.messages();
                assert_eq!(
                    messages
                        .iter()
                        .filter(|m| m["eventType"] == "text_delta")
                        .count(),
                    100
                );
                assert_eq!(
                    messages.iter().find(|m| m["type"] == "ready").unwrap()["semantics"]
                        ["messageUpdates"],
                    mode.as_str()
                );
                assert_rpc_update_mode(&messages, mode);
                assert!(messages.iter().any(|m| m["type"] == "agent_end"));
            }
        }
    }

    struct BackpressuredOutput {
        entered: Arc<std::sync::atomic::AtomicBool>,
        release: Arc<std::sync::atomic::AtomicBool>,
        output: RpcOutput,
    }

    impl Write for BackpressuredOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.entered
                .store(true, std::sync::atomic::Ordering::SeqCst);
            while !self.release.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.output.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_output_backpressure_is_bounded_and_keeps_workers_responsive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let release = Arc::new(AtomicBool::new(false));
        struct ReleaseOnDrop(Arc<AtomicBool>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _release = ReleaseOnDrop(release.clone());
        let entered = Arc::new(AtomicBool::new(false));
        let output = RpcOutput::default();
        let (writer, task, _) = start_rpc_writer(
            BackpressuredOutput {
                entered: entered.clone(),
                release: release.clone(),
                output: output.clone(),
            },
            crate::args::RpcMessageUpdates::default(),
        );
        write_line(&writer, &serde_json::json!({"id": 0})).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let producer_writer = writer.clone();
        let producer = tokio::spawn(async move {
            for id in 1..=100 {
                write_line(&producer_writer, &serde_json::json!({"id": id})).await;
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_millis(150),
            tokio::time::sleep(std::time::Duration::from_millis(20)),
        )
        .await
        .unwrap();
        assert!(
            !producer.is_finished(),
            "the bounded queue must apply backpressure"
        );
        release.store(true, Ordering::SeqCst);
        producer.await.unwrap();
        drop(writer);
        task.await.unwrap().unwrap();
        let messages = output.messages();
        assert_eq!(messages.len(), 101);
        for (index, message) in messages.iter().enumerate() {
            assert_eq!(message["id"], index);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_completion_response_follows_final_event_under_backpressure() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (harness, _) = rpc_test_harness("completed", 0.0).await;
        let (emitter, events) = rpi_agent::events::BroadcastEmitter::new(256);
        harness
            .set_agent_emitter(Some(Arc::new(emitter)))
            .await
            .unwrap();
        let output = RpcOutput::default();
        let release = Arc::new(AtomicBool::new(false));
        let release_writer = release.clone();
        let release_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            release_writer.store(true, Ordering::SeqCst);
        });
        let code = rpc_with_io(
            &harness,
            Some(events),
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(
                &b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n"[..],
            ),
            BackpressuredOutput {
                entered: Arc::new(AtomicBool::new(false)),
                release,
                output: output.clone(),
            },
        )
        .await;
        release_thread.join().unwrap();
        assert_eq!(code, 0);
        let messages = output.messages();
        let end = messages
            .iter()
            .position(|message| message["type"] == "agent_end")
            .unwrap();
        let settled = messages
            .iter()
            .position(|message| message["type"] == "agent_settled")
            .unwrap();
        assert!(end < settled);
        let run_id = messages[settled]["runId"].as_str().unwrap();
        assert!(!run_id.is_empty(), "settlement identifies its durable run");
        let records = harness
            .session()
            .find_records(&Default::default())
            .await
            .unwrap();
        assert!(records.iter().any(|record| matches!(record,
            rpi_harness::session::types::LaneRecord::OperationFinished(finished)
                if finished.run_id == run_id)));
        assert!(messages.iter().any(|message| {
            message["type"] == "response" && message["data"]["accepted"] == true
        }));
        assert!(messages.iter().any(|message| {
            message["type"] == "agent_settled" && message["outcome"] == "completed"
        }));
    }

    #[tokio::test]
    async fn rpc_output_failure_closes_without_waiting_for_more_input() {
        struct BrokenOutput;
        impl Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed client output",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (_client, input) = tokio::io::duplex(4096);
        let ui = rpi_extensions::UiDialogMailbox::new();
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            rpc_with_io(
                &harness,
                None,
                vec![],
                ui.clone(),
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input),
                BrokenOutput,
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, 1);
        assert!(!ui.is_attached());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_stop_aborts_and_joins_active_prompt() {
        use tokio::io::AsyncWriteExt;
        let (harness, provider) = rpc_test_harness("still streaming the response", 20.0).await;
        let lane = harness.lane("main");
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while provider
                .state()
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        client
            .write_all(b"{\"type\":\"stop\",\"id\":2}\n")
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let messages = output.messages();
        assert!(messages.iter().any(|message| {
            message["type"] == "agent_settled"
                && (message["outcome"] == "aborted" || message["outcome"] == "failed")
        }));
        assert_eq!(
            messages.iter().find(|m| m["id"] == 2).unwrap()["data"]["stopped"],
            true
        );
        assert!(
            lane.abort().await.is_err(),
            "RPC returned with an active lane"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_immediate_stop_cancels_prompt_before_or_after_lane_start() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("not completed", 20.0).await;
        let lane = harness.lane("main");
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client.write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n{\"type\":\"stop\",\"id\":2}\n").await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let messages = output.messages();
        let response = messages
            .iter()
            .find(|message| message["type"] == "response" && message["id"] == 1)
            .expect("the prompt has one correlated admission outcome");
        assert_eq!(
            messages
                .iter()
                .filter(|message| { message["type"] == "response" && message["id"] == 1 })
                .count(),
            1
        );
        if response["success"] == true {
            assert_eq!(response["data"]["accepted"], true);
            assert!(messages.iter().any(|message| {
                message["type"] == "agent_settled"
                    && (message["outcome"] == "aborted" || message["outcome"] == "failed")
            }));
        } else {
            // Stop may cancel the owned preflight BEFORE acceptance. In this
            // case there is no native run to settle, and no user is persisted.
            assert_eq!(
                response["error"].as_str(),
                Some("Operation aborted during prompt validation"),
                "{response}"
            );
            assert!(!messages
                .iter()
                .any(|message| message["type"] == "agent_start"));
        }
        assert!(messages.iter().any(|message| message["type"] == "response"
            && message["id"] == 2
            && message["success"] == true));
        assert!(lane.abort().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_dialog_answers_and_ping_are_read_while_prompt_runs() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) =
            rpc_test_harness("the streamed response finishes after the dialog", 30.0).await;
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
            .await
            .unwrap();
        ui.handle(serde_json::json!({"requestId":"approval", "toolCallId":"call", "ui":{}}))
            .unwrap();
        output.wait_for(|m| m["type"] == "ui_dialog_request").await;
        client.write_all(b"{\"type\":\"ui_dialog_response\",\"requestId\":\"approval\",\"answer\":true}\n{\"type\":\"ping\",\"id\":2}\n").await.unwrap();
        output.wait_for(|m| m["id"] == 2).await;
        assert_eq!(ui.poll("approval").unwrap()["answer"], true);
        output
            .wait_for(|m| m["type"] == "ui_dialog_closed" && m["requestId"] == "approval")
            .await;
        client.shutdown().await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(output.messages().iter().any(|message| {
            message["type"] == "agent_settled" && message["outcome"] == "completed"
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_notifies_client_when_an_approval_is_cancelled() {
        use tokio::io::AsyncWriteExt;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        ui.handle(serde_json::json!({"requestId":"cancelled", "toolCallId":"c", "ui":{}}))
            .unwrap();
        output.wait_for(|m| m["type"] == "ui_dialog_request").await;
        ui.cancel_tool_call("c");
        output
            .wait_for(|m| m["type"] == "ui_dialog_closed" && m["requestId"] == "cancelled")
            .await;
        // The notification must not consume the cancellation meant for the plugin.
        assert_eq!(ui.poll("cancelled").unwrap()["status"], "cancelled");
        client.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn rpc_accepts_native_dialog_responses_without_consuming_plugin_answers() {
        use crate::remote::protocol::RemoteCommand;
        use tokio::io::AsyncWriteExt;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        for (request_id, answer, response_id) in [
            (
                "answered",
                Some(serde_json::json!({"values":["No"],"kind":"selector"})),
                5,
            ),
            ("cancelled", None, 6),
        ] {
            ui.handle(serde_json::json!({"requestId":request_id,"toolCallId":request_id,"ui":{}}))
                .unwrap();
            output
                .wait_for(|m| m["type"] == "ui_dialog_request" && m["requestId"] == request_id)
                .await;
            let command = RemoteCommand::UiDialogResponse {
                request_id: request_id.into(),
                cancelled: answer.is_none(),
                answer,
            }
            .with_id(response_id);
            client
                .write_all(format!("{command}\n").as_bytes())
                .await
                .unwrap();
            let ack = output.wait_for(|m| m["id"] == response_id).await;
            assert_eq!(ack["data"]["accepted"], true);
            let result = ui.poll(request_id).unwrap();
            if request_id == "answered" {
                assert_eq!(result["answer"]["values"][0], "No");
            } else {
                assert_eq!(result["status"], "cancelled");
            }
        }
        client.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    fn assistant(text: &str, stop: StopReason) -> AssistantMessage {
        AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![Content::Text(TextContent {
                kind: TextContentType,
                text: text.into(),
                text_signature: None,
            })],
            api: rpi_ai::Api::AnthropicMessages,
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }
    }

    #[test]
    fn assistant_text_concatenates_text_blocks() {
        let m = assistant("hello", StopReason::Stop);
        assert_eq!(assistant_text(&m), "hello");
    }

    #[test]
    fn outcome_exit_code_maps_failed_aborted_to_1() {
        let failed = HarnessRunOutcome::Failed {
            leaf_id: "l".into(),
            error: OperationError {
                code: "boom".into(),
                message: "boom".into(),
            },
            final_entry_id: None,
            final_message: None,
        };
        assert_eq!(outcome_exit_code(&failed), 1);
        let completed = HarnessRunOutcome::Completed {
            leaf_id: "l".into(),
            final_entry_id: "e".into(),
            final_message: assistant("ok", StopReason::Stop),
        };
        assert_eq!(outcome_exit_code(&completed), 0);
    }

    #[test]
    fn run_end_outcome_str_roundtrip() {
        assert_eq!(run_end_outcome_str(RunEndOutcome::Completed), "completed");
        assert_eq!(run_end_outcome_str(RunEndOutcome::Aborted), "aborted");
        assert_eq!(run_end_outcome_str(RunEndOutcome::Failed), "failed");
    }

    #[test]
    fn agent_event_projection_keeps_terminal_and_tool_payloads() {
        let end = agent_event_json(&AgentEvent::AgentEnd { messages: vec![] });
        assert_eq!(end["type"], "agent_end");
        assert_eq!(end["messages"], serde_json::json!([]));

        let tool = agent_event_json(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            result: AgentToolResult::text("hello"),
            is_error: false,
        });
        assert_eq!(tool["type"], "tool_execution_end");
        assert_eq!(tool["result"]["content"][0]["text"], "hello");
        assert_eq!(tool["result"]["terminate"], false);

        let retry = agent_event_json(&AgentEvent::RetryScheduled {
            attempt: 3,
            max_retries: 10,
            delay_ms: 8_000,
            error: "503 service unavailable".into(),
        });
        assert_eq!(retry["type"], "retry_scheduled");
        assert_eq!(retry["attempt"], 3);
        assert_eq!(retry["maxRetries"], 10);
        assert_eq!(retry["delayMs"], 8_000);
        assert_eq!(retry["error"], "503 service unavailable");
    }
}

#[cfg(test)]
mod page_window_tests {
    use super::page_window;

    #[test]
    fn tail_is_kept_without_before_id() {
        // 12 items, limit 5 -> keep the last 5, more behind.
        assert_eq!(page_window(12, 5, None), (7..12, true));
        // Fewer items than the limit -> everything, nothing behind.
        assert_eq!(page_window(3, 5, None), (0..3, false));
    }

    #[test]
    fn before_id_opens_the_window_immediately_before_it() {
        // Page back from index 10 with limit 4 -> [6,10), more behind.
        assert_eq!(page_window(12, 4, Some(10)), (6..10, true));
        // Same, but reaching the very start -> no more behind.
        assert_eq!(page_window(12, 10, Some(10)), (0..10, false));
    }

    #[test]
    fn limit_larger_than_the_branch_never_underflows() {
        assert_eq!(page_window(5, 100, Some(2)), (0..2, false));
        assert_eq!(page_window(5, 100, None), (0..5, false));
    }
}

#[cfg(test)]
mod session_storage_notice_tests {
    use super::session_storage_notice;

    /// Print mode cannot recover a refused write by exporting: it is about to
    /// exit and offers no interactive `/export`. The message must say what was
    /// lost instead of naming a recovery the user cannot run from here.
    #[test]
    fn the_headless_notice_does_not_promise_an_unavailable_recourse() {
        let notice = session_storage_notice("disk full");
        assert!(notice.contains("disk full"), "{notice}");
        assert!(
            !notice.contains("/export"),
            "print mode has no interactive /export: {notice}"
        );
        assert!(
            notice.contains("print mode exits"),
            "the notice must say the turn is not coming back: {notice}"
        );
    }
}
