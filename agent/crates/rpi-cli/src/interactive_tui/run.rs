//! Prompt execution, completion reconciliation, compaction, and clipboard operations.

use super::*;

pub(super) async fn maybe_add_startup_changelog(
    harness: &AgentHarness,
    chat: &Arc<Container>,
    settings: &crate::settings::Settings,
    collapse: bool,
    existing_profile: bool,
) {
    let has_messages = harness
        .session()
        .find_entry_on_branch(
            &EntryQuery {
                entry_type: Some("message"),
                ..Default::default()
            },
            &Default::default(),
        )
        .await
        .map(|entry| entry.is_some())
        .unwrap_or(true);
    let entries = crate::changelog::bundled_entries();
    let baseline = crate::changelog::rpi_baseline(
        &entries,
        settings.last_rpi_changelog_version.as_deref(),
        settings.last_changelog_version.as_deref(),
    );
    let mut notice = crate::changelog::startup_changelog_for_profile(
        &entries,
        baseline,
        crate::VERSION,
        has_messages,
        existing_profile,
    );
    // Copy an identified rpi legacy baseline even when there is no new notice.
    // On resume this copies only the old version, so unseen updates stay pending.
    if notice.version_to_record.is_none() && settings.last_rpi_changelog_version.is_none() {
        notice.version_to_record = baseline.map(str::to_owned);
    }
    if let Some(markdown) = &notice.markdown {
        add_startup_changelog_message(chat, markdown, crate::VERSION, collapse);
    }
    if let Some(version) = notice.version_to_record {
        let saved = crate::settings::load_settings()
            .map_err(|error| error.to_string())
            .and_then(|mut current| {
                let candidate =
                    semver::Version::parse(&version).map_err(|error| error.to_string())?;
                if current
                    .last_rpi_changelog_version
                    .as_deref()
                    .and_then(|seen| semver::Version::parse(seen).ok())
                    .is_some_and(|seen| seen >= candidate)
                {
                    return Ok(());
                }
                current.last_rpi_changelog_version = Some(version);
                crate::settings::save_settings(&current)
            });
        if let Err(error) = saved {
            tracing::warn!("Could not record changelog version: {error}");
        }
    }
}

// ===========================================================================
// Run a single prompt (streaming or blocking)
// ===========================================================================

pub(super) fn launch_external_editor(draft: String, tx: mpsc::UnboundedSender<TuiMessage>) {
    std::thread::spawn(move || {
        let file = match tempfile::Builder::new()
            .prefix("rpi-draft-")
            .suffix(".md")
            .tempfile()
        {
            Ok(file) => file,
            Err(error) => {
                let _ = tx.send(TuiMessage::ExternalEditorResult(Err(format!(
                    "Could not create editor file: {error}"
                ))));
                return;
            }
        };
        if let Err(error) = std::fs::write(file.path(), draft.as_bytes()) {
            let _ = tx.send(TuiMessage::ExternalEditorResult(Err(format!(
                "Could not write editor file: {error}"
            ))));
            return;
        }
        let editor = std::env::var("RPI_EXTERNAL_EDITOR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| std::env::var("VISUAL").ok())
            .or_else(|| std::env::var("EDITOR").ok())
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "nano".to_string()
                }
            });
        let status = std::process::Command::new(&editor)
            .arg(file.path())
            .status();
        let result = match status {
            Ok(status) if status.success() => std::fs::read_to_string(file.path())
                .map_err(|error| format!("Could not read editor file: {error}")),
            Ok(status) => Err(format!("External editor exited with {status}")),
            Err(error) => Err(format!(
                "Could not launch external editor `{editor}`: {error}"
            )),
        };
        let _ = tx.send(TuiMessage::ExternalEditorResult(result));
    });
}

/// Apply the authoritative run result when it wins the race with the async
/// event drain, then detach the live component from further partial updates.
pub(super) fn reconcile_streamed_assistant_completion(
    current_assistant: &Arc<std::sync::Mutex<Option<Arc<AssistantMessageComponent>>>>,
    last_assistant_text: &Mutex<String>,
    final_message: Option<&AssistantMessage>,
) {
    let final_blocks = final_message.map(assistant_blocks);
    let final_text = final_message.map(assistant_text);
    let component = current_assistant.lock().unwrap().take();

    if let Some(component) = component {
        if let Some(blocks) = final_blocks.as_deref() {
            component.update_blocks(blocks);
        }
        component.set_streaming(false);
    }

    if let Some(text) = final_text.filter(|text| !text.is_empty()) {
        *last_assistant_text.lock().unwrap() = text;
    }
}

/// Drive a single prompt through the lane. When `streaming` is true, the
/// `AgentEvent` drain task renders the response live and the completed outcome
/// reconciles its final snapshot. When false (no `event_rx`), this falls back
/// to the blocking await-final-text path.

/// Drive a single prompt through the lane. When `streaming` is true, the
/// `AgentEvent` drain task renders the response live and the completed outcome
/// reconciles its final snapshot. When false (no `event_rx`), this falls back
/// to the blocking await-final-text path.
/// The TUI message for a refused session write.
///
/// Refusing to save is not a provider problem: retrying fails the same way, and
/// the messages on screen are the only copy. The advice is accurate because
/// `export_session` appends `AgentHarness::unsaved_messages` to the file — a
/// refused append never becomes a session entry, so an export that read only the
/// log would silently drop exactly the turns this warning is about.
pub(super) fn tui_session_storage_notice(error: &rpi_harness::result::HarnessError) -> String {
    format!(
        "The conversation could not be saved: {error}. Your messages are still on \
         screen, and /export writes the unsaved ones into the exported file — do \
         that before exiting."
    )
}

pub(super) async fn run_prompt_streaming(
    lane: &Arc<dyn AgentLane>,
    prompt: &str,
    resume: bool,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
    streaming: bool,
    images: Vec<rpi_ai::types::ImageContent>,
) {
    state.set_status(RunStatus::Working);
    tui.request_render(false);

    let outcome = if resume {
        // Continuing an interrupted run: no new user turn, just the provider call
        // the interrupted run never reached. upstream behaves the same way.
        match lane.resume_pending().await {
            Ok(Some(result)) => Ok(result),
            // Nothing to resume after all: leave the status alone and let the
            // normal prompt path take over.
            Ok(None) => {
                state.set_status(RunStatus::Idle);
                tui.request_render(false);
                return;
            }
            Err(error) => Err(error),
        }
    } else {
        lane.prompt_text(prompt, images).await
    };

    if streaming {
        // Broadcast delivery is asynchronous: the harness result can resolve
        // before the drain task processes MessageEnd. Reconcile from the
        // authoritative outcome before detaching the component so the final
        // streamed tail cannot be left at an earlier partial snapshot.
        let final_message = match &outcome {
            Ok(result) => match &result.outcome {
                HarnessRunOutcome::Completed { final_message, .. }
                | HarnessRunOutcome::Aborted { final_message, .. } => Some(final_message),
                HarnessRunOutcome::Failed { final_message, .. } => final_message.as_ref(),
                HarnessRunOutcome::Suspended { .. } => None,
            },
            Err(_) => None,
        };
        reconcile_streamed_assistant_completion(
            &state.current_assistant,
            &state.last_assistant_text,
            final_message,
        );
    }

    state.set_status(RunStatus::Idle);
    // Let the run's own writes reach the disk before this function returns.
    // A forced exit (`emergency_exit`) bypasses the shutdown sequence entirely,
    // and it is only taken while a run is in flight — i.e. exactly when there is
    // work that has not been persisted yet. Draining here bounds that window to
    // one run instead of the whole session.
    lane.drain_session().await;
    // Guarantee the pending-messages display reconciles at the end of every
    // run/prompt — covers aborts and the non-streaming path, where no
    // `AgentEnd` event necessarily reaches the drain task.
    refresh_pending_messages(state, lane).await;
    // The blocking path has no drain task, so `AgentEnd` never runs and this is
    // the only place its write failures can surface.
    if super::events::report_write_failures(&state.notice_container(), lane) {
        state
            .history_page
            .unsaved
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    // Footer repair for a dropped `MessageEnd` used to live here, but that call
    // raced the drain: the final assistant message can already be in the log
    // while its `MessageEnd` is still queued, so the rebuild counted turns the
    // drain was about to count again — and a distinct turn with the same
    // timestamp and usage is indistinguishable from that delayed duplicate by
    // content alone. Reconciliation now runs inside the drain's `AgentEnd`
    // handler, which the FIFO broadcast turns into a consumption barrier: every
    // event queued before it has been applied, so a `set_usage` rebuild can
    // never be followed by a delayed duplicate of what it counted.
    if let Ok(result) = &outcome {
        // The context badge keeps its run-outcome fallback: it shows one turn's
        // measurement (set semantics, nothing accumulated), so racing the drain
        // costs nothing — the drain's own `MessageEnd` handling just overwrites
        // it. On the non-streaming path there is no drain at all, and this is
        // the only place the badge gets a value from.
        let final_message: Option<&AssistantMessage> = match &result.outcome {
            HarnessRunOutcome::Completed { final_message, .. }
            | HarnessRunOutcome::Aborted { final_message, .. } => Some(final_message),
            HarnessRunOutcome::Failed { final_message, .. } => final_message.as_ref(),
            HarnessRunOutcome::Suspended { .. } => None,
        };
        if let Some(a) = final_message {
            record_context_usage(&state.footer, a);
        }
    }

    match outcome {
        Ok(result) => match &result.outcome {
            HarnessRunOutcome::Failed {
                error,
                final_message,
                ..
            } => {
                // Only add an error line if the stream did NOT already render
                // an assistant message for it (drain task leaves
                // current_assistant Some only on an abrupt end).
                // A final assistant error is emitted by the event drain only
                // in streaming mode. In regular mode there is no drain task,
                // so suppressing this branch merely hides 405/auth/network
                // diagnostics from the user.
                let already_rendered = streaming && final_message.is_some();
                if !already_rendered {
                    let msg = final_message
                        .as_ref()
                        .and_then(|m| m.error_message.clone())
                        .unwrap_or_else(|| format!("{error:?}"));
                    add_error_message(&state.notice_container(), &msg);
                }
            }
            HarnessRunOutcome::Suspended { .. } => {
                add_error_message(
                    &state.notice_container(),
                    "Run suspended (deferred) — resume is not supported in v1.",
                );
            }
            HarnessRunOutcome::Aborted { final_message, .. } => {
                // Aborted runs render their own partial/final message via the
                // stream; only add a note on the blocking fallback path.
                if !streaming {
                    add_error_message(&state.notice_container(), "Request aborted.");
                    let _ = final_message; // (rendered by the stream in streaming mode)
                }
            }
            HarnessRunOutcome::Completed { final_message, .. } => {
                if !streaming {
                    let text = assistant_text(final_message);
                    if !text.is_empty() {
                        add_assistant_message_for_state(state, &text);
                        *state.last_assistant_text.lock().unwrap() = text;
                    }
                }
            }
        },
        Err(e) => {
            // A refused session write is not a provider problem: retrying will fail
            // the same way, and the transcript on screen is the only copy left.
            if e.tag() == rpi_harness::result::HarnessErrorTag::SessionStorage {
                state
                    .history_page
                    .unsaved
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                add_error_message(&state.notice_container(), &tui_session_storage_notice(&e));
            } else {
                add_error_message(&state.notice_container(), &e.to_string());
            }
        }
    }

    if !streaming {
        history::settle(state, tui);
    }
    tui.request_render(false);
}

/// Drive an explicitly selected skill through the live transcript path.
/// `AgentLane::skill` performs the canonical `<skill>` invocation; this helper
/// keeps the TUI status, pending queue, final assistant message, and errors in
/// sync with ordinary prompts.
pub(super) async fn run_skill_streaming(
    lane: &Arc<dyn AgentLane>,
    name: &str,
    additional_instructions: Option<&str>,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
    streaming: bool,
) {
    state.set_status(RunStatus::Working);
    tui.request_render(false);
    let outcome = lane.skill(name, additional_instructions).await;
    if streaming {
        let final_message = match &outcome {
            Ok(result) => match &result.outcome {
                HarnessRunOutcome::Completed { final_message, .. }
                | HarnessRunOutcome::Aborted { final_message, .. } => Some(final_message),
                HarnessRunOutcome::Failed { final_message, .. } => final_message.as_ref(),
                HarnessRunOutcome::Suspended { .. } => None,
            },
            Err(_) => None,
        };
        reconcile_streamed_assistant_completion(
            &state.current_assistant,
            &state.last_assistant_text,
            final_message,
        );
    }

    state.set_status(RunStatus::Idle);
    refresh_pending_messages(state, lane).await;

    match outcome {
        Ok(result) => match result.outcome {
            HarnessRunOutcome::Completed { final_message, .. } => {
                if !streaming {
                    let text = assistant_text(&final_message);
                    if !text.is_empty() {
                        add_assistant_message_for_state(state, &text);
                        *state.last_assistant_text.lock().unwrap() = text;
                    }
                }
            }
            HarnessRunOutcome::Aborted { .. } => {
                if !streaming {
                    add_error_message(&state.notice_container(), "Skill invocation aborted.");
                }
            }
            HarnessRunOutcome::Suspended { .. } => add_error_message(
                &state.notice_container(),
                "Skill invocation suspended (deferred) — resume is not supported in v1.",
            ),
            HarnessRunOutcome::Failed {
                error,
                final_message,
                ..
            } => {
                if !(streaming && final_message.is_some()) {
                    let message = final_message
                        .as_ref()
                        .and_then(|m| m.error_message.clone())
                        .unwrap_or_else(|| format!("{error:?}"));
                    add_error_message(&state.notice_container(), &message);
                }
            }
        },
        Err(error) => add_error_message(&state.notice_container(), &error.to_string()),
    }
    if !streaming {
        history::settle(state, tui);
    }
    tui.request_render(false);
}

/// `/compact`: drive a compaction on the lane (mirrors TS `app.compact`).
/// Reports the outcome as a transcript note; the harness progress watcher
/// displays the compaction indicator while the summary is generated.
pub(super) async fn run_compact(
    lane: &Arc<dyn AgentLane>,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
) {
    state.set_status(RunStatus::Working);
    tui.request_render(false);
    match lane.compact(None).await {
        Ok(_) => {
            add_note_message(&state.notice_container(), "Conversation compacted.");
            // The last assistant usage describes the pre-compaction context;
            // reset the badge to `?` until the next response reports the new
            // (much smaller) prompt size — mirrors pi's `percent: null`.
            state.footer.set_context_tokens(None);
        }
        Err(e) => {
            add_error_message(&state.notice_container(), &format!("Compact failed: {e}"));
        }
    }
    state.set_status(RunStatus::Idle);
    history::settle(state, tui);
    tui.request_render(false);
}

/// Copy the fullscreen mouse selection, if present. `Some(false)` still
/// means the selection handled the action: never silently copy another source
/// when the clipboard write fails. Injection keeps tests off the OS clipboard.
pub(super) fn copy_mouse_selection_with(
    tui: &TuiAltScreen,
    copy: impl FnOnce(&str) -> bool,
) -> Option<bool> {
    tui.selection_text().map(|text| copy(&text))
}

/// `/copy`: copy the last assistant reply to the clipboard. Best-effort —
/// when no clipboard is available (or the `clipboard` feature is off), prints a
/// hint instead. Mirrors the TS `/copy` (copies `this.messages.at(-1)` text).
pub(super) fn copy_last_assistant(state: &Arc<TuiState>) {
    let text = state.last_assistant_text.lock().unwrap().clone();
    if text.is_empty() {
        add_note_message(
            &state.notice_container(),
            "Nothing to copy yet — no assistant reply captured.",
        );
        return;
    }
    if copy_to_clipboard(&text) {
        add_note_message(
            &state.notice_container(),
            "Copied last reply to the clipboard.",
        );
    } else {
        // Clipboard unavailable — print the text to the transcript so the user
        // can select/copy it manually (degrades gracefully in headless envs).
        let preview: String = text.chars().take(200).collect();
        add_note_message(
            &state.notice_container(),
            &format!(
                "Clipboard unavailable. Last reply: {preview}{}",
                if text.chars().count() > 200 {
                    "…"
                } else {
                    ""
                }
            ),
        );
    }
}

/// Run `f` against one process-lifetime clipboard instance.
///
/// On X11 the clipboard is served by the process that owns it: dropping the
/// `arboard::Clipboard` instance destroys the serving window, so a
/// create-write-drop pattern loses the data whenever no desktop clipboard
/// manager took it over (arboard only waits 100ms for a `CLIPBOARD_MANAGER`
/// handoff, and sessions like LXQt/Openbox run none). The instance therefore
/// lives in a `static` and outlives every copy; Windows and macOS keep
/// OS-owned clipboards and are immune, but sharing one handle is harmless
/// there too, so all platforms take the same path. A failed first creation
/// (headless machine) is retried on later calls, matching the previous
/// per-call behavior.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
fn with_shared_clipboard<T>(f: impl FnOnce(&mut arboard::Clipboard) -> T) -> Option<T> {
    static SHARED: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);
    let mut slot = SHARED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = arboard::Clipboard::new().ok();
    }
    Some(f(slot.as_mut()?))
}

/// Best-effort clipboard write. Enabled only with the `clipboard` feature
/// (`arboard`) on non-Android platforms; otherwise returns `false` so the
/// caller degrades to a hint.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
pub(super) fn copy_to_clipboard(text: &str) -> bool {
    with_shared_clipboard(|clipboard| clipboard.set_text(text).is_ok()).unwrap_or(false)
}

#[cfg(any(not(feature = "clipboard"), target_os = "android"))]
pub(super) fn copy_to_clipboard(_text: &str) -> bool {
    false
}

/// Best-effort clipboard text read for Ctrl+V paste. The optional `clipboard`
/// feature keeps headless builds free of platform clipboard dependencies;
/// on Android the OS owns clipboard access so this degrades to `None`.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
pub(super) fn read_clipboard_text() -> Option<String> {
    with_shared_clipboard(|clipboard| clipboard.get_text().ok()).flatten()
}

#[cfg(any(not(feature = "clipboard"), target_os = "android"))]
pub(super) fn read_clipboard_text() -> Option<String> {
    None
}

/// Read a clipboard bitmap and normalize it to PNG for the provider-neutral
/// `ImageContent` contract. The optional clipboard feature keeps headless
/// builds free of platform clipboard dependencies.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
pub(super) fn read_clipboard_image() -> Result<Option<rpi_ai::types::ImageContent>, String> {
    let image = with_shared_clipboard(|clipboard| clipboard.get_image())
        .ok_or_else(|| "clipboard unavailable".to_string())?;
    let image = match image {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    let width =
        u32::try_from(image.width).map_err(|_| "clipboard image is too wide".to_string())?;
    let height =
        u32::try_from(image.height).map_err(|_| "clipboard image is too tall".to_string())?;
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err("clipboard image dimensions are outside the supported range".into());
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&image.bytes)
            .map_err(|e| e.to_string())?;
    }
    Ok(Some(rpi_ai::types::ImageContent {
        kind: rpi_ai::types::ImageContentType,
        data: base64::engine::general_purpose::STANDARD
            .encode(bytes)
            .into(),
        mime_type: "image/png".into(),
    }))
}

/// Finder copied files arrive as aliases, rather than clipboard bitmaps.
#[cfg(all(target_os = "macos", feature = "clipboard"))]
pub(super) fn read_clipboard_file_paths() -> Option<Vec<String>> {
    let script = "try\nset items to the clipboard as alias list\nset paths to {}\nrepeat with itemPath in items\nset end of paths to POSIX path of itemPath\nend repeat\nset AppleScript's text item delimiters to linefeed\nreturn paths as text\non error\nreturn \"\"\nend try";
    let output = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let paths: Vec<String> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// Explorer's copied files use CF_HDROP rather than a bitmap or plain text.
#[cfg(all(windows, feature = "clipboard"))]
pub(super) fn read_clipboard_file_paths() -> Option<Vec<String>> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::UI::Shell::DragQueryFileW;
    const CF_HDROP: u32 = 15;

    struct ClipboardRead;
    impl Drop for ClipboardRead {
        fn drop(&mut self) {
            // SAFETY: this guard exists only after OpenClipboard succeeds.
            unsafe {
                CloseClipboard();
            }
        }
    }

    // SAFETY: null HWND is supported; the clipboard is opened only for reading.
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        return None;
    }
    let _guard = ClipboardRead;
    // SAFETY: the clipboard remains open while its borrowed HDROP is queried.
    let drop = unsafe { GetClipboardData(CF_HDROP) };
    if drop.is_null() {
        return None;
    }
    let count = unsafe { DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0) };
    let mut paths = Vec::new();
    for index in 0..count {
        // SAFETY: query the length first, then provide that many UTF-16 units
        // plus the terminator. No DragFinish: the handle belongs to the clipboard.
        let length = unsafe { DragQueryFileW(drop, index, std::ptr::null_mut(), 0) };
        if length == 0 {
            continue;
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied =
            unsafe { DragQueryFileW(drop, index, buffer.as_mut_ptr(), buffer.len() as u32) };
        if copied == 0 {
            continue;
        }
        let path = String::from_utf16(&buffer[..copied as usize]).ok()?;
        paths.push(path);
    }
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

#[cfg(not(all(any(target_os = "macos", windows), feature = "clipboard")))]
pub(super) fn read_clipboard_file_paths() -> Option<Vec<String>> {
    None
}

pub(super) fn attach_pasted_images(state: &Arc<TuiState>, text: &str) -> bool {
    let Some(images) = images_from_pasted_paths(text) else {
        return false;
    };
    let count = images.len();
    for image in images {
        state.queue_image(image);
    }
    add_note_message(
        &state.notice_container(),
        &format!("Attached {count} image(s) to the next prompt."),
    );
    true
}

/// Consume a paste as attachments only when every path is an actual image.
/// Otherwise leave the pasted text intact. Handles Finder escaped spaces,
/// quoted multiple paths and clipboard file paths separated by newlines.
pub(super) fn images_from_pasted_paths(text: &str) -> Option<Vec<rpi_ai::types::ImageContent>> {
    let candidate = text.trim().trim_matches(['\"', '\'']);
    let single = std::path::Path::new(candidate);
    if single.is_file() {
        return crate::app::image_content_from_path(single)
            .ok()
            .flatten()
            .map(|image| vec![image]);
    }
    let mut paths = Vec::new();
    if text.contains('\n') {
        paths.extend(
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| line.trim().trim_matches(['\"', '\'']).to_owned()),
        );
    } else {
        let mut path = String::new();
        let mut quote = None;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\'' | '"' if quote == Some(ch) => quote = None,
                '\'' | '"' if quote.is_none() => quote = Some(ch),
                '\\' if quote != Some('\'')
                    && chars.peek().is_some_and(|ch| {
                        ch.is_whitespace() || matches!(ch, '\'' | '"' | '\\')
                    }) =>
                {
                    path.push(chars.next().unwrap());
                }
                ch if ch.is_whitespace() && quote.is_none() => {
                    if !path.is_empty() {
                        paths.push(std::mem::take(&mut path));
                    }
                }
                _ => path.push(ch),
            }
        }
        if quote.is_some() {
            return None;
        }
        if !path.is_empty() {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return None;
    }
    paths
        .iter()
        .map(|path| {
            crate::app::image_content_from_path(std::path::Path::new(path))
                .ok()
                .flatten()
        })
        .collect()
}

pub(super) fn user_message_with_images(
    text: &str,
    images: Vec<rpi_ai::types::ImageContent>,
) -> AgentMessage {
    let content = if images.is_empty() {
        rpi_ai::types::UserContent::Text(text.into())
    } else {
        let mut blocks = Vec::with_capacity(images.len() + 1);
        if !text.is_empty() {
            blocks.push(rpi_ai::types::Content::text(text));
        }
        blocks.extend(images.into_iter().map(rpi_ai::types::Content::Image));
        rpi_ai::types::UserContent::Blocks(blocks)
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    AgentMessage::User(UserMessage::new(content, timestamp))
}

pub(super) fn add_image_previews(
    chat: &Arc<Container>,
    images: &[rpi_ai::types::ImageContent],
    show: bool,
) {
    for image in images {
        add_image_preview(chat, image, show);
    }
}

pub(super) fn add_user_images(
    chat: &Arc<Container>,
    user: &rpi_ai::types::UserMessage,
    show: bool,
) {
    if let rpi_ai::types::UserContent::Blocks(blocks) = &user.content {
        for block in blocks {
            if let rpi_ai::types::Content::Image(image) = block {
                add_image_preview(chat, image, show);
            }
        }
    }
}

pub(super) fn add_tool_images(
    chat: &Arc<Container>,
    content: &[rpi_agent::types::TextContentOrImage],
    show: bool,
) {
    for block in content {
        if let rpi_agent::types::TextContentOrImage::Image(image) = block {
            add_image_preview(
                chat,
                &rpi_ai::types::ImageContent {
                    kind: rpi_ai::types::ImageContentType,
                    data: image.data.clone(),
                    mime_type: image.mime_type.clone(),
                },
                show,
            );
        }
    }
}

pub(super) fn add_image_preview(
    chat: &Arc<Container>,
    image: &rpi_ai::types::ImageContent,
    show: bool,
) {
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&*image.data) {
        // Kitty's f=100 payload must be PNG, regardless of the model MIME type.
        let bytes = if image.mime_type == "image/png" {
            bytes
        } else {
            match rpi_tools::image_processing::process_image(&bytes, Default::default()) {
                Ok(png) => png,
                Err(_) => {
                    add_note_message(chat, "[Image preview unavailable]");
                    return;
                }
            }
        };
        let mut options = ImageOptions::default();
        options.width = Some(60);
        options.alt_text = Some("[Attached image]".into());
        let preview = Arc::new(Image::from_data(bytes, options));
        preview.set_inline_visible(show);
        chat.add_child(preview);
        chat.add_child(Arc::new(Spacer::new(1)));
    }
}

#[cfg(any(not(feature = "clipboard"), target_os = "android"))]
pub(super) fn read_clipboard_image() -> Result<Option<rpi_ai::types::ImageContent>, String> {
    Ok(None)
}

/// Keep the non-streaming fallback in the same live slot as streaming output.
/// In particular, running a prompt while browsing an old page must not mutate it.
pub(super) fn add_assistant_message_for_state(state: &TuiState, text: &str) {
    let container = if state.history_page.is_bound() {
        &state.history_page.active
    } else {
        &state.chat_container
    };
    add_assistant_message_blocking(container, text, state.markdown_transformer());
}

/// Blocking fallback (no `event_rx`): render the final assistant text as a
/// single `AssistantMessageComponent`, mirroring the pre-streaming behavior.
/// `transformer` is the live assistant-markdown transformer (B5e); `None` is
/// the identity path. The blocking path only fires when `event_rx` is absent,
/// so it shares the same transformer the streaming path installs on its
/// components.
pub(super) fn add_assistant_message_blocking(
    container: &Arc<Container>,
    text: &str,
    transformer: Option<MarkdownTransformer>,
) {
    if text.is_empty() {
        return;
    }
    let msg = Arc::new(AssistantMessageComponent::new(
        AssistantMessageOptions::default(),
    ));
    if let Some(t) = &transformer {
        msg.set_markdown_transformer(Some(t.clone()));
    }
    msg.update_text(text);
    container.add_child(msg);
    container.add_child(Arc::new(Spacer::new(1)));
}
