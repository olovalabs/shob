//! Agent event rendering and queued-message recovery.

use super::*;

// ===========================================================================
// AgentEvent drain task — the streaming core
// ===========================================================================

/// Drain `AgentEvent`s from the broadcast receiver and apply the TS
/// `handleEvent` event→UI mapping. Runs on a `tokio::spawn`'d task for the
/// lifetime of the TUI.
/// Re-read the lane's queue and update the pending-messages dock slot.
/// Cheap (two short mutex locks) and a no-op when the snapshot is unchanged,
/// so it is safe to call on every relevant event.
pub(super) async fn refresh_pending_messages(state: &Arc<TuiState>, lane: &Arc<dyn AgentLane>) {
    if let Ok(queued) = lane.queued_messages().await {
        state.set_pending_queue(queued);
    }
}

/// Whether the transcript holds a panel that repaints its elapsed readout from
/// the clock on every frame: a running bash command, or a tool call still in
/// [`ToolStatus::Running`]. Such a panel needs a real transcript rebuild each
/// render tick — a frame that reuses the cached scroll content would leave its
/// timer frozen at the second of the last full rebuild.
pub(super) fn transcript_has_live_panel(
    bash_components_present: bool,
    tool_components: &HashMap<String, Arc<ToolExecutionComponent>>,
) -> bool {
    bash_components_present
        || tool_components
            .values()
            .any(|component| component.status() == ToolStatus::Running)
}

/// Prepend restored queued messages to the draft the user may already have
/// typed (upstream's `restoreQueuedMessagesToEditor` places them first).
pub(super) fn merge_queued_into_draft(queued_text: &str, current: &str) -> String {
    if current.trim().is_empty() {
        queued_text.to_string()
    } else {
        format!("{queued_text}\n\n{current}")
    }
}

/// Place the caret at the END of `text` after `set_text`. `set_text` resets the
/// caret to `(0, 0)`, and the last line's **byte** length is the right column:
/// a char count was wrong for multibyte drafts, and row 0 was wrong for a
/// multi-line one (upstream's `setTextInternal(text, "end")`).
pub(super) fn set_editor_text_caret_at_end(editor: &Arc<Editor>, text: &str) {
    editor.set_text(text);
    let last_row = text.split('\n').count().saturating_sub(1);
    let last_len = text.split('\n').next_back().map(str::len).unwrap_or(0);
    editor.set_cursor(last_row, last_len);
}

/// Drain every queued steering/follow-up message out of the lane and restore it
/// to the editor (upstream's `restoreQueuedMessagesToEditor`). Returns how many
/// were restored; the queue is consumed either way, so the dock drops its rows.
///
/// Runs on a spawned task rather than through the [`TuiMessage`] mailbox: the
/// async main loop owns `run_prompt_streaming(..).await` for the whole run, and
/// a queue only exists *while* a run is in flight — so a `TuiMessage::Dequeue`
/// was never serviced until the run ended, after the lane had already consumed
/// the queue. That made Alt+Q a no-op while it was most needed.
pub(super) async fn restore_queued_messages_to_editor(
    lane: Arc<dyn AgentLane>,
    editor: Arc<Editor>,
    state: Arc<TuiState>,
    tui: Arc<TuiAltScreen>,
) -> usize {
    match lane.clear_queue().await {
        Ok(queued) => {
            // The editor holds **one** draft, so its attachments are one strip.
            // (The per-message pairing matters only for consumers that restore
            // a *list* of queued entries — see `QueuedMessageView`.)
            let images: Vec<_> = queued.iter().flat_map(|m| m.images.clone()).collect();
            state.restore_pending_images(images);
            let all: Vec<String> = queued.iter().map(|m| m.text.clone()).collect();
            if !all.is_empty() {
                let queued_text = all.join("\n\n");
                // Read the EXPANDED draft: `set_text` below clears the folded
                // paste store, so any `[paste #id …]` marker still in the
                // editor must be materialized first or it would be left behind
                // as literal text.
                let current = editor.get_expanded_text();
                let combined = merge_queued_into_draft(&queued_text, &current);
                set_editor_text_caret_at_end(&editor, &combined);
            }
            // The lane drained the queue; refresh so the dock drops the rows.
            refresh_pending_messages(&state, &lane).await;
            tui.request_render(false);
            all.len()
        }
        Err(error) => {
            add_error_message(
                &state.notice_container(),
                &format!("Could not restore queued messages: {error}"),
            );
            tui.request_render(false);
            0
        }
    }
}

/// `app.message.dequeue`: restore the queued messages and report the count, like
/// upstream's `handleDequeue` (`Restored N queued message(s)` / `No queued
/// messages to restore`). The status is a dim transcript note (rpi's analog of
/// native `showStatus`).
pub(super) async fn handle_dequeue(
    lane: Arc<dyn AgentLane>,
    editor: Arc<Editor>,
    state: Arc<TuiState>,
    tui: Arc<TuiAltScreen>,
) {
    let restored =
        restore_queued_messages_to_editor(lane, editor, state.clone(), tui.clone()).await;
    if restored == 0 {
        add_note_message(&state.notice_container(), "No queued messages to restore");
    } else {
        let plural = if restored > 1 { "s" } else { "" };
        add_note_message(
            &state.notice_container(),
            &format!("Restored {restored} queued message{plural} to editor"),
        );
    }
    tui.request_render(false);
}

/// Cancel the active run the native-pi way: pull every queued steering /
/// follow-up message back into the editor FIRST, then abort. upstream's
/// `onEscape` does exactly this during streaming
/// (`restoreQueuedMessagesToEditor({abort: true})`), so a cancel never silently
/// swallows messages the user staged while the run was in flight — they land in
/// the editor to be re-edited or resent.
pub(super) fn abort_run_restoring_queue(
    lane: Arc<dyn AgentLane>,
    editor: Arc<Editor>,
    state: Arc<TuiState>,
    tui: Arc<TuiAltScreen>,
) {
    tokio::spawn(async move {
        let _ = restore_queued_messages_to_editor(lane.clone(), editor, state, tui.clone()).await;
        let _ = lane.abort().await;
        tui.request_render(false);
    });
}

pub(super) async fn drain_agent_events(
    mut rx: broadcast::Receiver<AgentEvent>,
    tui: Arc<TuiAltScreen>,
    state: Arc<TuiState>,
    chat: Arc<Container>,
    lane: Arc<dyn AgentLane>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => handle_agent_event(event, &tui, &state, &chat, &lane).await,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // `MessageUpdate` carries a full snapshot of the block list, so
                // the transcript re-syncs on its own. `MessageEnd` does NOT —
                // it is a terminal event with no later carrier, so the footer
                // counters accumulated from it (usage totals, cache-hit rate)
                // would stay frozen. Record the gap so the run's end-of-turn
                // reconciliation knows to rebuild from the durable log instead
                // of trusting what the drain managed to observe.
                tracing::debug!(
                    skipped,
                    "agent event broadcast lagged; run-end footer reconciliation required"
                );
                state
                    .events_lagged
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Apply a single `AgentEvent` to the UI. Mirrors the TS `handleEvent` switch
/// (`interactive-mode.ts:3068-3396`).
pub(super) async fn handle_agent_event(
    event: AgentEvent,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
    chat: &Arc<Container>,
    lane: &Arc<dyn AgentLane>,
) {
    match event {
        AgentEvent::AgentStart => {
            state.set_status(RunStatus::Working);
            tui.request_render(false);
        }

        AgentEvent::AgentEnd { .. } => {
            // Finalize any still-streaming assistant message.
            state.transcript_view().finalize_assistant();
            state.set_status(RunStatus::Idle);
            // Flush any `!command` results that completed mid-run: the run's
            // tool sequence is closed, so appending now keeps transcript order
            // intact (upstream's `flushPendingBashMessages`).
            let pending: Vec<AgentMessage> =
                std::mem::take(&mut *state.pending_bash_messages.lock().unwrap());
            for message in pending {
                if let Err(error) = lane.append_message(message).await {
                    state
                        .history_page
                        .unsaved
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    add_error_message(
                        &state.notice_container(),
                        &format!("Could not record bash result: {error}"),
                    );
                }
            }
            // The run drained the queue; refresh so consumed entries drop out
            // of the pending-messages display.
            refresh_pending_messages(state, lane).await;
            // A write that failed mid-run is not a run failure, so it cannot ride
            // the run outcome. Report it here: inside the alternate screen a
            // `tracing::warn!` reaches nobody, and the user must know before
            // exiting that the conversation stopped being saved.
            if report_write_failures(&state.notice_container(), lane) {
                state
                    .history_page
                    .unsaved
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            // Authoritative footer reconciliation when the broadcast dropped
            // events (`Lagged`). `AgentEnd` is the barrier that makes a rebuild
            // here safe: the broadcast preserves order, so at this point every
            // event queued before it — including every `MessageEnd` this run
            // emitted — has already been through this handler, and no event of
            // this run can arrive after it to be counted twice. Rebuilding only
            // on the lagged path keeps an ordinary turn off the whole-branch
            // read; a dropped `MessageEnd` is terminal, so the frozen counters
            // have no later carrier to heal them and need the durable log.
            if state
                .events_lagged
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                reconcile_footer_from_lane(lane, state).await;
            }
            history::settle(state, tui);
            tui.request_render(false);
        }

        AgentEvent::RetryScheduled {
            attempt,
            max_retries,
            delay_ms,
            ..
        } => {
            state.show_retry(attempt, max_retries, delay_ms);
            tui.request_render(false);
        }

        AgentEvent::TurnStart => {
            // A new turn: reset the streaming-assistant guard so the next
            // MessageStart creates a fresh component.
            state.transcript_view().finalize_assistant();
        }

        AgentEvent::TurnEnd {
            message,
            tool_results,
            ..
        } => {
            // Finalize the assistant message for this turn.
            let view = state.transcript_view();
            if let AgentMessage::Assistant(a) = &message {
                view.finalize_assistant_blocks(&assistant_blocks(a));
            } else {
                view.finalize_assistant();
            }
            // Tool results whose components were never ended by a
            // ToolExecutionEnd are handled by the normal path; there is nothing
            // extra to finalize here.
            let _ = tool_results;
            tui.request_render(false);
        }

        AgentEvent::MessageStart { message, .. } => match message {
            AgentMessage::Assistant(a) => {
                // The shared transcript view renders this through the same
                // component path as the remote client. It installs the live
                // markdown transformer + hide-thinking preference itself.
                state.transcript_view().apply(
                    &UiEvent::AssistantStart {
                        blocks: assistant_blocks(&a),
                    },
                    tui.width(),
                );
                tui.request_render(false);
            }
            AgentMessage::Custom(custom) => {
                let payload = serde_json::json!({
                    "customType": custom.role,
                    "content": custom.content,
                    "details": custom.data,
                    "expanded": false,
                    "outputPad": 1,
                });
                if let Some(component) = extension_message_component(
                    &state.extension_session,
                    &custom.role,
                    &payload,
                    state.markdown_transformer(),
                ) {
                    chat.add_child(component);
                    chat.add_child(Arc::new(Spacer::new(1)));
                    tui.request_render(false);
                } else {
                    add_note_message(&state.notice_container(), &custom_message_fallback(&custom));
                    tui.request_render(false);
                }
            }
            // Queued (steering / follow-up) user messages are rendered here,
            // not at queue time, so a message that is still waiting never looks
            // delivered. The loop drains those itself and emits
            // `message_start` for them (`agent_loop.rs` pending-message drain).
            //
            // A DIRECTLY-sent prompt does NOT reach this arm: the harness
            // persists it up front and runs the loop with an empty prompts vec,
            // so no `message_start` is emitted for it — the submit handler
            // renders that bubble instead.
            AgentMessage::User(user) => {
                state.transcript_view().add_user(&user_message_text(&user));
                add_user_images(&state.chat_container, &user, state.images_visible());
                // A consumed entry must drop out of the pending display.
                refresh_pending_messages(state, lane).await;
                tui.request_render(false);
            }
            // ToolResult / other starts are echoed via the tool-execution
            // components; ignore the dupes.
            _ => {}
        },

        AgentEvent::MessageUpdate {
            message,
            assistant_message_event,
            ..
        } => {
            let _ = assistant_message_event; // the snapshot is authoritative
            let a: &rpi_ai::types::AssistantMessage = &message;
            let text = assistant_text(a);
            // The shared view creates/updates tool + bash panels from the
            // snapshot's finalized tool calls and streams the block list, so a
            // thinking block renders live exactly as the remote client does.
            state.transcript_view().apply(
                &UiEvent::AssistantUpdate {
                    blocks: assistant_blocks(a),
                    tool_calls: assistant_tool_calls(a),
                },
                tui.width(),
            );
            // A freshly-created bash panel hides the global `Working…` loader.
            state.sync_working_loader_with_bash();
            *state.last_assistant_text.lock().unwrap() = text;
            tui.request_render(false);
        }

        AgentEvent::MessageEnd { message, .. } => {
            if let AgentMessage::Assistant(a) = &message {
                let text = assistant_text(a);
                state
                    .transcript_view()
                    .finalize_assistant_blocks(&assistant_blocks(a));
                // Cache the finalized text for `/copy`.
                if !text.is_empty() {
                    *state.last_assistant_text.lock().unwrap() = text;
                }
                // Cache-miss notice (`maybeShowCacheMissNotice`): the previous
                // turn's input established a cacheable prefix; a prompt that
                // re-reads less than it should means the prefix was re-billed.
                // Uses the shared cache-stats logic (noise floor, idle gap,
                // model change) and prices the miss when known.
                let usage = &a.usage;
                // Footer usage totals + cache hit rate (pi footer.ts). Unconditional
                // accumulation: the duplicate-suppression problem is handled at the
                // reconciliation site instead — reconciliation only runs inside this
                // drain (see the `AgentEnd` arm), where the FIFO broadcast guarantees
                // every event queued before it has already been through this handler.
                // A rebuild that replaced the totals can therefore never be followed
                // by a still-queued duplicate of a turn it already counted.
                state.footer.add_usage(
                    usage.input,
                    usage.output,
                    usage.cache_read,
                    usage.cache_write,
                    usage.cost.total,
                );
                let denom = usage.input + usage.cache_read + usage.cache_write;
                if denom > 0 && (usage.cache_read + usage.cache_write) > 0 {
                    state
                        .footer
                        .set_cache_hit_rate(Some(usage.cache_read as f64 / denom as f64 * 100.0));
                }
                // Context-window badge (`?/512k` → `63.2%/512k`): the provider
                // reports the prompt token count for this request, which is the
                // context size the footer displays.
                record_context_usage(&state.footer, a);
                let miss = state
                    .cache_tracker
                    .lock()
                    .unwrap()
                    .observe(a, &rpi_harness::cache_stats::NoPrices);
                // upstream gates these behind `showCacheMissNotices`
                // (default `false`); the tracker still runs so the footer's
                // cache-hit rate stays accurate either way.
                let cache_miss_notices = *state.cache_miss_notices.lock().unwrap();
                if let Some(miss) = miss.filter(|_| cache_miss_notices) {
                    let cost = if miss.missed_cost > 0.0 {
                        format!(" (~${:.4})", miss.missed_cost)
                    } else {
                        String::new()
                    };
                    add_note_message(
                        &state.notice_container(),
                        &format!(
                            "Cache miss: {} tokens re-billed{}",
                            format_tokens(miss.missed_tokens),
                            cost
                        ),
                    );
                }
                if let Some(text) = extension_usage_text(Some(&state.extension_session), usage) {
                    add_note_message(&state.notice_container(), &text);
                }
                // Error assistant messages carry the provider diagnostic in
                // `error_message`, not in text content. The assistant
                // component is empty for these messages, so surface the
                // diagnostic as a visible error row in the transcript.
                if let Some(error) = assistant_error_text(a) {
                    add_error_message(&state.notice_container(), &error);
                }
            }
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            state.transcript_view().apply(
                &UiEvent::ToolStart {
                    id: tool_call_id,
                    name: tool_name,
                    args,
                },
                tui.width(),
            );
            state.sync_working_loader_with_bash();
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => {
            state.transcript_view().apply(
                &UiEvent::ToolUpdate {
                    id: tool_call_id,
                    name: tool_name,
                    args,
                    result: crate::remote::protocol::tool_result_json(&partial_result),
                },
                tui.width(),
            );
            state.sync_working_loader_with_bash();
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            state.transcript_view().apply(
                &UiEvent::ToolEnd {
                    id: tool_call_id,
                    name: tool_name,
                    result: crate::remote::protocol::tool_result_json(&result),
                    is_error,
                },
                tui.width(),
            );
            add_tool_images(
                &state.chat_container,
                &result.content,
                state.images_visible(),
            );
            state.sync_working_loader_with_bash();
            // Bash tool completion is user-visible: immediate render ensures the
            // result is displayed even when the scheduler thread is busy or the
            // throttle window has not elapsed.
            tui.render_now(false);
        }
    }
}

/// Update the footer's context-window badge from one assistant response.
///
/// `Usage::context_tokens()` is the prompt size the provider reported for that
/// request — the value pi's footer renders as `{percent}%/{window}`. Aborted
/// and errored turns are skipped (their usage is not a real context
/// measurement, mirroring pi's `lastAssistantUsageInfo`) so the last valid
/// count stays on screen instead of flickering to `?`.
pub(super) fn record_context_usage(
    footer: &rpi_tui::FooterComponent,
    message: &rpi_ai::AssistantMessage,
) {
    if message.stop_reason == rpi_ai::StopReason::Aborted
        || message.stop_reason == rpi_ai::StopReason::Error
    {
        return;
    }
    let tokens = message.usage.context_tokens();
    if tokens > 0 {
        footer.set_context_tokens(Some(tokens));
    }
}

/// Fold the session's recorded usage into the footer, replacing whatever the
/// live `MessageEnd` path accumulated. Only called from the drain's `AgentEnd`
/// handler: the FIFO broadcast makes that call a consumption barrier, so every
/// event queued before it has already been applied and a rebuild here can never
/// be followed by a delayed duplicate of a turn it counted.
///
/// The live path (`handle_agent_event`) can only add what it actually observed.
/// An `AgentEvent` broadcast that overflows reports `Lagged` and drops events
/// outright, and `MessageEnd` is terminal — unlike `MessageUpdate`, it has no
/// later snapshot that re-delivers the same information. So a single dropped
/// `MessageEnd` froze `↑/↓/R/W/$` and `CH%` for the rest of the session, while
/// the context badge kept moving because it had its own fallback.
///
/// This rebuilds every counter from the durable log, which is the authority the
/// replay path (`render_session_history`) reads too — so a live session and a
/// resumed one agree on the numbers by construction. `set_*` (replace) rather
/// than `add_usage` (accumulate) keeps it idempotent: it runs on top of a footer
/// that usually already holds the same totals.
///
/// Best-effort: the footer is cosmetic, so a failed read leaves it untouched
/// instead of failing the turn.
pub(super) async fn reconcile_footer_from_lane(lane: &Arc<dyn AgentLane>, state: &Arc<TuiState>) {
    match lane.session_view().branch_overview().await {
        Ok(overview) => history::apply_overview(&state.footer, &overview),
        Err(error) => tracing::debug!(%error, "could not read branch overview"),
    }
}

/// Return the diagnostic carried by a failed or aborted provider request.
/// Providers may omit `error_message`; keep a stable fallback so a terminal
/// request failure can never render as an empty transcript turn.
pub(super) fn assistant_error_text(message: &rpi_ai::AssistantMessage) -> Option<String> {
    let fallback = match message.stop_reason {
        rpi_ai::StopReason::Error => "Provider request failed.",
        rpi_ai::StopReason::Aborted => "Request aborted.",
        // A `length` stop is not an error, but a silent cut is indistinguishable
        // from an interrupted message. Reasoning models make this common: a long
        // chain of thought can consume the whole output budget (DeepSeek bills
        // reasoning tokens inside `max_tokens`), so the answer arrives empty or
        // clipped. Say why it stopped instead of leaving the turn looking broken.
        rpi_ai::StopReason::Length => {
            "Response was truncated at the output-token limit (max_tokens) before it finished. \
             Lower the thinking level or raise the model's maxTokens."
        }
        _ => return None,
    };
    Some(
        message
            .error_message
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(fallback)
            .to_string(),
    )
}

/// Whether a tool result opts into markdown rendering of its body.
///
/// A tool signals this by setting `details.markdown = true` on its result
/// (the `rpi-todo` extension does this for its progress bar + table payload).
/// The host then hands the body to the markdown renderer instead of printing
/// it as plain text — see `ToolExecutionComponent::set_result_markdown`.
/// Absent or non-`true` details keep the existing plain-text rendering, so
/// every other tool is unaffected.
pub(super) fn tool_result_requests_markdown(details: Option<&serde_json::Value>) -> bool {
    details
        .and_then(|details| details.get("markdown"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Render the session writes that failed since the last call.
///
/// Deduplicated by (`what`, `message`) so a backend failing the same way on every
/// turn shows one line rather than one per turn: the queue is drained every
/// `AgentEnd`, and the same disk-full error would otherwise repeat forever.
pub(super) fn report_write_failures(chat: &Arc<Container>, lane: &Arc<dyn AgentLane>) -> bool {
    let failures = lane.take_write_failures();
    if failures.is_empty() {
        return false;
    }
    let mut seen: Vec<(String, String)> = Vec::new();
    for failure in failures {
        let key = (failure.what.clone(), failure.message.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        add_error_message(
            chat,
            &format!(
                "Session not saved: could not persist {} ({}). \\
                 Your messages are still on screen, and /export now writes the \\
                 unsaved ones into the exported file — do that before exiting: \\
                 the session log cannot be repaired.",
                failure.what, failure.message
            ),
        );
    }
    true
}
