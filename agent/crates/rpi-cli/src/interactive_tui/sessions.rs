//! Session switching, import, fork, sharing, and transcript history rendering.

use super::*;

/// `/share`: mirror the TS intent (share the session). With the `gh` CLI on
/// PATH, create a gist of the exported markdown; otherwise fall back to the
/// clipboard (best-effort) and note the local path.
pub(super) async fn share_session(harness: &AgentHarness, chat: &Arc<Container>) {
    use std::process::Stdio;

    // Reuse the export builder for the transcript text.
    let tree = harness.session().view("main");
    let entries = match tree
        .find_entries(&EntryQuery {
            // Exports append entries top-to-bottom, so use chronological order
            // instead of the session query default (newest-first).
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
    {
        Ok(e) => e,
        Err(e) => {
            add_error_message(chat, &format!("Could not read session: {e}"));
            return;
        }
    };
    let mut md = String::from("# Session\n\n");
    for e in entries {
        let Entry::Message(me) = e else { continue };
        match &me.message {
            AgentMessage::User(u) => {
                md.push_str(&format!("## User\n\n{}\n\n", user_message_text(u)));
            }
            AgentMessage::Assistant(a) => {
                let text = assistant_text(a);
                if !text.is_empty() {
                    md.push_str(&format!("## Assistant\n\n{}\n\n", text));
                }
            }
            _ => {}
        }
    }

    // `gh gist create` — stdin-piped, best-effort; only when gh exists.
    let gh = std::process::Command::new("gh")
        .arg("gist")
        .arg("create")
        .arg("--filename")
        .arg("session.md")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = gh {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(md.as_bytes());
            let _ = stdin.flush();
        }
        let out = child.wait_with_output().ok();
        if let Some(out) = out {
            if out.status.success() {
                let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
                add_note_message(chat, &format!("Shared session: {url}"));
                return;
            }
        }
        add_note_message(chat, "gh gist failed — falling back to the clipboard.");
    } else {
        add_note_message(chat, "gh CLI not found — falling back to the clipboard.");
    }
    // Clipboard fallback (or transcript echo when the clipboard feature is off).
    if copy_to_clipboard(&md) {
        add_note_message(chat, "Session transcript copied to the clipboard.");
    } else {
        add_note_message(
            chat,
            "Clipboard unavailable — use /export to write the transcript to a file.",
        );
    }
}

// Session export is driven through the product-layer `AgentSession`
// (`crate::agent_session`), which owns format selection + default filenames —
// the TUI loop calls `AgentSession::export*` directly, so there is no separate
// helper here.

/// Fork the current session into a new JSONL session and switch to it (TS
/// `/fork` — a copy of the transcript in a fresh file; the fork is a new
/// session the user continues in). Uses the repo's `fork_typed`, then swaps
/// the harness backing and renders the (empty-ish) fork transcript.
/// Hot-switch the harness to another saved session: abort any in-flight run,
/// open the target session file, swap the durable backing, and re-render the
/// transcript from the new history (mirrors pi's `/session` resume-in-place).
/// Shared by the `/session` selector, `/import`, and `/fork`. The session's own
/// last-used model is replayed ([`restore_session_model`]) and its recorded
/// usage folded back into the footer ([`render_session_history`]).
pub(super) async fn switch_to_session(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
    id: &str,
    cwd: &std::path::Path,
    chat: &Arc<Container>,
    state: &Arc<TuiState>,
    tool_context: &rpi_extensions::ToolCallContext,
    catalog: &Arc<std::sync::RwLock<Vec<rpi_ai::Model>>>,
) -> bool {
    state.history_page.invalidate();
    if *state.status.lock().unwrap() == RunStatus::Working {
        state.set_status(RunStatus::Aborting);
        let _ = lane.abort().await;
    }
    if let Err(error) = lane.wait_for_idle().await {
        add_error_message(
            &state.notice_container(),
            &format!("Could not switch session: {error}"),
        );
        return false;
    }
    // Same late-result race as `/new`: a running user `!command` finishes into
    // the live lane, which would be the NEW session after this swap.
    if state.user_bash_running() {
        add_note_message(
            &state.notice_container(),
            "Wait for the !command to finish before switching sessions, or press Esc to cancel it.",
        );
        return false;
    }
    let cwd_str = cwd.to_string_lossy().to_string();
    match crate::session::open_session_by_id_in_dir(id, &cwd_str, &state.session_dir).await {
        Ok(restored) => {
            // Route through the funnel so the plugin channels follow the swap;
            // a bare `set_session` would leave every plugin naming the session
            // it was serving before this call.
            if let Err(error) =
                crate::session::set_active_session(harness, restored.session, Some(tool_context))
                    .await
            {
                add_error_message(
                    &state.notice_container(),
                    &format!("Could not switch session: {error}"),
                );
                state.set_status(RunStatus::Idle);
                return false;
            }
            // Replay the target session's own model. Without this the lane
            // keeps whatever `/model` last selected, so a session resumed
            // after a temporary switch silently runs on the wrong model — the
            // session's own `ModelChange`/assistant entries are the authority.
            restore_session_model(harness, lane, state, catalog).await;
            chat.clear();
            state.reset_session_state();
            let (tools, skills, context_files) = current_capabilities(harness, lane).await;
            add_welcome_message_with_capabilities(
                &state.history_page.header,
                &tools,
                &skills,
                &context_files,
            );
            history::restore(harness, state, chat).await;
            state.set_status(RunStatus::Idle);
            add_note_message(
                &state.notice_container(),
                &format!("Switched to session {id}."),
            );
            // The tools' working directory is fixed by the run that built them;
            // a hot-switch cannot move it (that would take a reload-level
            // rebuild of the tool env, system prompt, and project resources).
            // Say where the session belongs instead of implying the move.
            match &restored.recorded_cwd {
                Some(dir) if dir != cwd => add_note_message(
                    &state.notice_container(),
                    &format!(
                        "This session was created in {}. Tools still run in {}; \
                         restart rpi in that directory to work there.",
                        dir.display(),
                        cwd.display()
                    ),
                ),
                None => add_note_message(
                    &state.notice_container(),
                    "This session's recorded directory no longer exists; \
                     tools keep using the current one.",
                ),
                _ => {}
            }
            true
        }
        Err(e) => {
            state.set_status(RunStatus::Idle);
            add_error_message(
                &state.notice_container(),
                &format!("Could not open session {id}: {e}"),
            );
            false
        }
    }
}

/// `/import <path>`: validate and save a session in the selected repository,
/// then activate it through the same funnel as other session switches.
pub(super) async fn import_session(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
    path: &str,
    cwd: &std::path::Path,
    chat: &Arc<Container>,
    state: &Arc<TuiState>,
    tool_context: &rpi_extensions::ToolCallContext,
    catalog: &Arc<std::sync::RwLock<Vec<rpi_ai::Model>>>,
) {
    state.history_page.invalidate();
    use std::path::Path as FsPath;

    let src = FsPath::new(path);
    if !src.is_file() {
        add_error_message(
            &state.notice_container(),
            &format!("Import source not found: {path}"),
        );
        return;
    }
    let Some(fname) = src.file_name().and_then(|f| f.to_str()) else {
        add_error_message(&state.notice_container(), "Import source has no file name.");
        return;
    };
    if !fname.ends_with(".jsonl") {
        add_error_message(
            &state.notice_container(),
            "Import source must be a .jsonl session file.",
        );
        return;
    }
    if *state.status.lock().unwrap() == RunStatus::Working {
        state.set_status(RunStatus::Aborting);
        let _ = lane.abort().await;
    }
    if let Err(error) = lane.wait_for_idle().await {
        add_error_message(
            &state.notice_container(),
            &format!("Could not import session: {error}"),
        );
        return;
    }
    // Same late-result race as `/new`: a running user `!command` finishes into
    // the live lane, which would be the NEW session after this swap.
    if state.user_bash_running() {
        add_note_message(
            &state.notice_container(),
            "Wait for the !command to finish before importing a session, or press Esc to cancel it.",
        );
        return;
    }
    let result =
        crate::session::import_session_file(src, &cwd.to_string_lossy(), &state.session_dir).await;
    let new_session = match result {
        Ok(session) => session,
        Err(error) => {
            state.set_status(RunStatus::Idle);
            add_error_message(
                &state.notice_container(),
                &format!("Could not import session: {error}"),
            );
            return;
        }
    };
    if let Err(error) =
        crate::session::set_active_session(harness, new_session, Some(tool_context)).await
    {
        state.set_status(RunStatus::Idle);
        add_error_message(
            &state.notice_container(),
            &format!("Could not import session: {error}"),
        );
        return;
    }
    chat.clear();
    state.reset_session_state();
    let (tools, skills, context_files) = current_capabilities(harness, lane).await;
    add_welcome_message_with_capabilities(
        &state.history_page.header,
        &tools,
        &skills,
        &context_files,
    );
    // Same replay as `/session`: the imported session's own history names the
    // model it was last using.
    restore_session_model(harness, lane, state, catalog).await;
    history::restore(harness, state, chat).await;
    state.set_status(RunStatus::Idle);
    add_note_message(
        &state.notice_container(),
        &format!("Imported session from {path}"),
    );
}

pub(super) async fn fork_session(
    harness: &AgentHarness,
    cwd: &std::path::Path,
    chat: &Arc<Container>,
    state: &Arc<TuiState>,
    tool_context: &rpi_extensions::ToolCallContext,
) {
    state.history_page.invalidate();
    let lane = harness.lane("main");
    if *state.status.lock().unwrap() == RunStatus::Working {
        state.set_status(RunStatus::Aborting);
        let _ = lane.abort().await;
    }
    if let Err(error) = lane.wait_for_idle().await {
        add_error_message(
            &state.notice_container(),
            &format!("Could not fork session: {error}"),
        );
        return;
    }
    // Same late-result race as `/new`: a running user `!command` finishes into
    // the live lane, which would be the NEW session after this swap.
    if state.user_bash_running() {
        add_note_message(
            &state.notice_container(),
            "Wait for the !command to finish before forking a session, or press Esc to cancel it.",
        );
        return;
    }
    let new_session = match crate::session::fork_session_storage(
        harness,
        &cwd.to_string_lossy(),
        &state.session_dir,
    )
    .await
    {
        Ok(session) => session,
        Err(error) => {
            state.set_status(RunStatus::Idle);
            add_error_message(
                &state.notice_container(),
                &format!("Could not fork session: {error}"),
            );
            return;
        }
    };
    if let Err(error) =
        crate::session::set_active_session(harness, new_session, Some(tool_context)).await
    {
        state.set_status(RunStatus::Idle);
        add_error_message(
            &state.notice_container(),
            &format!("Could not fork session: {error}"),
        );
        return;
    }
    chat.clear();
    state.reset_session_state();
    let (tools, skills, context_files) = current_capabilities(harness, &lane).await;
    add_welcome_message_with_capabilities(
        &state.history_page.header,
        &tools,
        &skills,
        &context_files,
    );
    // A fork continues the CURRENT conversation: a `/model` pick made after the
    // last recorded turn is still the user's live intent, and `apply_model_selection`
    // deliberately persists nothing for the replay to read. Restoring the older
    // recorded model here would silently undo that pick, so the fork keeps the
    // live model instead of replaying.
    history::restore(harness, state, chat).await;
    state.set_status(RunStatus::Idle);
    add_note_message(&state.notice_container(), "Forked into a new session.");
}

/// `/new` (alias `/clear`): begin a fresh session.
///
/// Creates a new JSONL session file, swaps it onto the harness through the
/// shared [`crate::session::set_active_session`] funnel (so plugin channels
/// follow the swap), rebuilds the transcript with the session's REAL capability
/// snapshot, and drops every per-session counter in the footer.
///
/// The persistence policy of the run is honoured: when the current session is
/// ephemeral (`--no-session`), the fresh session is another in-memory one —
/// nothing may land on disk, and `/new` must never turn an explicit
/// no-persistence choice into persistent chat history.
///
/// The session being left behind is **not** deleted: its file stays on disk and
/// remains reachable through `/session`. Removing user data is not something
/// `/new` should do implicitly.
///
/// Returns `true` when the harness now serves a new session.
pub(super) async fn start_new_session(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
    cwd: &std::path::Path,
    chat: &Arc<Container>,
    state: &Arc<TuiState>,
    tool_context: &rpi_extensions::ToolCallContext,
) -> bool {
    state.history_page.invalidate();
    if *state.status.lock().unwrap() == RunStatus::Working {
        state.set_status(RunStatus::Aborting);
        let _ = lane.abort().await;
    }
    if let Err(error) = lane.wait_for_idle().await {
        add_error_message(
            &state.notice_container(),
            &format!("Could not start a new session: {error}"),
        );
        state.set_status(RunStatus::Idle);
        return false;
    }
    // A user `!command` writes its result through the live lane when it
    // finishes (`UserBashFinished`), with no originating-session identity.
    // Swapping the session underneath it would record that result in the NEW
    // conversation, so the switch waits for the shell run to finish. Merely
    // cancelling would not close the race: the completion is still coming.
    if state.user_bash_running() {
        add_note_message(
            &state.notice_container(),
            "Wait for the !command to finish before starting a new session, or press Esc to cancel it.",
        );
        return false;
    }
    let ephemeral = harness.session().storage().is_ephemeral();
    let new_session = if ephemeral {
        // `--no-session` chose no persistence; the fresh session honours that
        // choice instead of silently writing a JSONL file to disk.
        crate::session::ephemeral_session()
    } else {
        let dir = &state.session_dir;
        if let Err(error) = std::fs::create_dir_all(dir) {
            add_error_message(
                &state.notice_container(),
                &format!(
                    "Could not create session dir {}: {error} — check write permissions",
                    dir.display()
                ),
            );
            state.set_status(RunStatus::Idle);
            return false;
        }
        let cwd_str = cwd.to_string_lossy().to_string();
        match crate::session::create_jsonl_session(dir, &cwd_str).await {
            Ok(session) => session,
            Err(error) => {
                add_error_message(
                    &state.notice_container(),
                    &format!(
                        "Could not create session: {error} — the current session is still active"
                    ),
                );
                state.set_status(RunStatus::Idle);
                return false;
            }
        }
    };
    let new_id = new_session.storage().metadata().id.clone();
    // Route through the funnel so the plugin channels follow the swap; a bare
    // `set_session` would leave every plugin naming the session it was serving
    // before this call.
    if let Err(error) =
        crate::session::set_active_session(harness, new_session, Some(tool_context)).await
    {
        add_error_message(
            &state.notice_container(),
            &format!(
                "Could not start a new session: {error} — the previous session is still active"
            ),
        );
        state.set_status(RunStatus::Idle);
        return false;
    }
    state.reset_session_state();
    chat.clear();
    let (tools, skills, context_files) = current_capabilities(harness, lane).await;
    add_welcome_message_with_capabilities(
        &state.history_page.header,
        &tools,
        &skills,
        &context_files,
    );
    history::restore(harness, state, chat).await;
    state.set_status(RunStatus::Idle);
    if ephemeral {
        add_note_message(
            &state.notice_container(),
            "Started a new ephemeral session (--no-session).",
        );
    } else {
        add_note_message(
            &state.notice_container(),
            &format!("Started new session {new_id}."),
        );
    }
    true
}

/// Render the restored session's prior transcript (user + assistant messages)
/// into the chat container. Called at TUI startup for `--continue`/`--resume`/
/// `--session` launches; a no-op for fresh sessions (no entries). Best-effort:
/// any session read failure just starts with an empty transcript.
///
/// `transformer` is the live assistant-markdown transformer (B5e); `None` is
/// the identity path. Each restored assistant component installs it so replayed
/// history renders through the same `register_markdown_transformer` handlers
/// the live stream does.
///
/// `footer` is the footer whose per-session counters the replayed usage belongs
/// to. It is `None` at startup, where the footer has not been built yet.
#[cfg(test)]
pub(super) async fn render_session_history(
    harness: &AgentHarness,
    chat: &Arc<Container>,
    transformer: Option<MarkdownTransformer>,
    extension_session: Option<crate::session::ExtensionSessionCell>,
    show_images: bool,
    footer: Option<&rpi_tui::FooterComponent>,
) {
    let tree = harness.session().view("main");
    let Ok(page) = tree.branch_entry_page(&Default::default()).await else {
        return;
    };
    render_history_entries(
        page.entries,
        chat,
        transformer,
        extension_session,
        show_images,
    );
    restore_history_panels(&tree, chat).await;
    if let Some(footer) = footer {
        if let Ok(name) = tree.get_name().await {
            footer.set_session_name(name.as_deref());
        }
        if let Ok(overview) = tree.branch_overview().await {
            history::apply_overview(footer, &overview);
        }
    }
}

/// Render complete entries without dropping variants or truncating large entries.
pub(super) fn render_history_entries(
    entries: Vec<Entry>,
    chat: &Arc<Container>,
    transformer: Option<MarkdownTransformer>,
    extension_session: Option<crate::session::ExtensionSessionCell>,
    show_images: bool,
) {
    for e in entries {
        match e {
            Entry::Message(me) => match &me.message {
                AgentMessage::User(u) => {
                    add_user_message(chat, &user_message_text(u));
                    add_user_images(chat, u, show_images);
                }
                AgentMessage::Assistant(a) => {
                    let comp = Arc::new(AssistantMessageComponent::new(
                        AssistantMessageOptions::default(),
                    ));
                    comp.set_show_images(show_images);
                    comp.set_trailing_spacing(1);
                    if let Some(t) = &transformer {
                        comp.set_markdown_transformer(Some(t.clone()));
                    }
                    comp.update_blocks(&assistant_blocks(a));
                    chat.add_child(comp);
                    // The component adds spacing only when it has visible content.
                    if let Some(text) = extension_usage_text(extension_session.as_ref(), &a.usage) {
                        add_note_message(chat, &text);
                    }
                    if let Some(error) = assistant_error_text(a) {
                        add_error_message(chat, &error);
                    }
                }
                AgentMessage::ToolResult(result) => {
                    // Current panels are independent of the visible history page.
                    if matches!(
                        result.tool_name.as_str(),
                        "plan_mode_start" | "plan_mode_complete" | "todo"
                    ) && !result.is_error
                    {
                        continue;
                    }
                    // Tool results are persisted as separate message entries,
                    // not as part of the assistant text. Restore them as
                    // completed tool panels so resumed sessions show the
                    // command output as well as the user's prompts.
                    let comp = Arc::new(ToolExecutionComponent::new(&result.tool_name, ""));
                    comp.set_result(&tool_result_message_text(result), result.is_error);
                    if tool_result_requests_markdown(result.details.as_ref()) {
                        comp.set_result_markdown(true);
                    }
                    chat.add_child(comp);
                    for block in &result.content {
                        if let rpi_ai::types::Content::Image(image) = block {
                            add_image_preview(chat, image, show_images);
                        }
                    }
                    // The tool panel carries its own trailing spacer now (one
                    // convention for every transcript entry) — no host spacer.
                }
                AgentMessage::Custom(custom) => {
                    if let Some(session) = &extension_session {
                        if let Some(component) = extension_message_component(
                            session,
                            &custom.role,
                            &serde_json::json!({
                                "customType": custom.role,
                                "content": custom.content,
                                "details": custom.data,
                            }),
                            transformer.clone(),
                        ) {
                            chat.add_child(component);
                            chat.add_child(Arc::new(Spacer::new(1)));
                            continue;
                        }
                    }
                    add_note_message(chat, &custom_message_fallback(custom));
                }
            },
            Entry::Compaction(compaction) => {
                add_note_message(
                    chat,
                    &format!(
                        "Compacted {} tokens: {}",
                        compaction.tokens_before, compaction.summary
                    ),
                );
            }
            Entry::BranchSummary(summary) => {
                add_note_message(chat, &format!("Branch summary: {}", summary.summary));
            }
            Entry::Custom(custom) => {
                if matches!(
                    custom.custom_type.as_str(),
                    "plan-mode" | "todo" | "todo-state"
                ) {
                    continue;
                }
                let rendered = extension_session.as_ref().and_then(|session| {
                    extension_entry_component(session, &custom.custom_type, custom.data.clone())
                });
                if let Some(component) = rendered {
                    chat.add_child(component);
                    chat.add_child(Arc::new(Spacer::new(1)));
                } else if let Some(text) =
                    custom_entry_display_text(&custom.custom_type, custom.data.as_ref())
                {
                    add_note_message(chat, &text);
                }
            }
            Entry::ModelChange(change) => {
                add_note_message(
                    chat,
                    &format!("Model changed to {}:{}", change.provider, change.model_id),
                );
            }
            Entry::ThinkingLevel(change) => {
                add_note_message(
                    chat,
                    &format!("Thinking level: {:?}", change.thinking_level),
                );
            }
            Entry::ActiveTools(change) => {
                add_note_message(
                    chat,
                    &format!("Active tools: {}", change.active_tool_names.join(", ")),
                );
            }
        }
    }
}

/// Current panels are restored with storage-side custom-type filters and limits.
pub(super) async fn restore_history_panels(
    tree: &Arc<dyn rpi_harness::session::types::SessionTree>,
    chat: &Arc<Container>,
) {
    // Read at most one entry per supported persisted kind, then replay in
    // sequence order: legacy todo-state must not overwrite a newer todo entry.
    let mut latest = Vec::new();
    for kind in ["plan-mode", "todo", "todo-state"] {
        let Ok(entries) = tree
            .find_entries_on_branch(
                &EntryQuery {
                    custom_type: Some(kind.into()),
                    order: Some(EntryOrder::NewestFirst),
                    limit: Some(1),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
        else {
            continue;
        };
        latest.extend(entries);
    }
    latest.sort_by_key(Entry::seq);
    for entry in latest {
        let Entry::Custom(custom) = entry else {
            continue;
        };
        let mut details = custom.data.unwrap_or(serde_json::Value::Null);
        if custom.custom_type == "plan-mode" {
            if let Some(fields) = details.as_object_mut() {
                fields.insert("kind".into(), serde_json::json!("plan"));
            }
            crate::transcript_view::update_plan_panel(
                chat,
                &serde_json::json!({"details":details}),
            );
        } else {
            crate::transcript_view::update_todo_list(chat, &serde_json::json!({"details":details}));
        }
    }
}

/// Apply the model a freshly-opened session was last using.
///
/// The session log is the authority: `ModelChange` entries and every assistant
/// turn record the provider/model that produced them. Without this replay the
/// lane keeps whatever the previous session (or a temporary `/model` pick) left
/// behind, so switching back to an older session silently ran it on the wrong
/// model while the footer still showed the new one.
///
/// The derivation is **branch-scoped**: only the lane's selected ancestry
/// counts (`find_entries_on_branch` from the lane leaf), never the session-wide
/// entry list — an abandoned sibling branch or a child lane's later turn must
/// not select the model.
///
/// Best-effort: a session with no recorded model, or a model that is no longer
/// in the authenticated catalog, leaves the current lane model untouched rather
/// than failing the switch. Resuming a session must not fail because one of its
/// models became unavailable.
pub(super) async fn restore_session_model(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
    state: &Arc<TuiState>,
    catalog: &Arc<std::sync::RwLock<Vec<rpi_ai::Model>>>,
) {
    let _ = harness; // Shared switch helper signature.
    let Ok(overview) = lane.session_view().branch_overview().await else {
        return;
    };
    let Some((provider, model_id)) = overview.model else {
        return;
    };
    let models = catalog
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let Some(model) = models
        .iter()
        .find(|m| m.provider == provider && m.id.eq_ignore_ascii_case(&model_id))
        .cloned()
    else {
        return;
    };
    if lane.set_model(model.clone()).await.is_ok() {
        state.set_current_model(&model);
    }
}

pub(super) fn invoke_extension_renderer(
    session: &crate::session::ExtensionSessionCell,
    kind: rpi_extensions::RegisteredRendererKind,
    payload: &serde_json::Value,
) -> Option<serde_json::Value> {
    let snapshot = session.lock().ok()?.snapshot_arc()?;
    let input = serde_json::to_string(payload).ok()?;
    for renderer in snapshot.renderers_of(kind) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut out = rpi_plugin_sdk::StbString::empty();
            let rc = (renderer.render_fn)(
                rpi_plugin_sdk::StbStringRef::from_str(&input),
                &mut out as *mut rpi_plugin_sdk::StbString,
                renderer.user_data,
            );
            // SAFETY: the callback transfers a live output to its producer.
            let out = unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(
                    out,
                    Some(renderer.plugin_free_string),
                )
            };

            if rc == 0 {
                Some(out.to_string_lossy())
            } else {
                None
            }
        }))
        .ok()
        .flatten();
        let Some(text) = outcome else { continue };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            return Some(value);
        }
    }
    None
}

pub(super) fn extension_text_component(
    value: &serde_json::Value,
) -> Option<Arc<dyn rpi_tui::Component>> {
    if let Some(lines) = value.get("lines").and_then(|v| v.as_array()) {
        let text = lines
            .iter()
            .filter_map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        return Some(Arc::new(Text::new(text, 0, 0)));
    }
    let text = value.get("text").and_then(|v| v.as_str())?;
    if value.get("markdown").and_then(|v| v.as_bool()) == Some(true) {
        let component = Arc::new(AssistantMessageComponent::new(
            AssistantMessageOptions::default(),
        ));
        component.update_blocks(&[AssistantBlock::Text(text.to_string())]);
        Some(component)
    } else {
        Some(Arc::new(Text::new(text, 0, 0)))
    }
}

pub(super) fn extension_message_component(
    session: &crate::session::ExtensionSessionCell,
    custom_type: &str,
    payload: &serde_json::Value,
    transformer: Option<MarkdownTransformer>,
) -> Option<Arc<dyn rpi_tui::Component>> {
    let value = invoke_extension_renderer(
        session,
        rpi_extensions::RegisteredRendererKind::Message,
        payload,
    )?;
    if value.get("markdown").and_then(|v| v.as_bool()) == Some(true) {
        let text = value.get("text").and_then(|v| v.as_str())?;
        let component = Arc::new(AssistantMessageComponent::new(
            AssistantMessageOptions::default(),
        ));
        if let Some(transformer) = transformer {
            component.set_markdown_transformer(Some(transformer));
        }
        component.update_blocks(&[AssistantBlock::Text(text.to_string())]);
        return Some(component);
    }
    extension_text_component(&value)
        .or_else(|| Some(Arc::new(Text::new(format!("[{custom_type}]"), 0, 0))))
}

/// Render usage from a completed assistant message through the registered
/// message renderers. Hosts without a token-usage renderer return `None`.
pub(super) fn extension_usage_text(
    session: Option<&crate::session::ExtensionSessionCell>,
    usage: &rpi_ai::types::Usage,
) -> Option<String> {
    let session = session?;
    let payload = serde_json::json!({
        "customType": "token-usage",
        "usage": usage,
    });
    let value = invoke_extension_renderer(
        session,
        rpi_extensions::RegisteredRendererKind::Message,
        &payload,
    )?;
    value
        .get("text")
        .and_then(|value| value.as_str())
        .filter(|text| !text.trim().is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn extension_entry_component(
    session: &crate::session::ExtensionSessionCell,
    custom_type: &str,
    data: Option<serde_json::Value>,
) -> Option<Arc<dyn rpi_tui::Component>> {
    let payload = serde_json::json!({
        "customType": custom_type,
        "data": data,
    });
    let value = invoke_extension_renderer(
        session,
        rpi_extensions::RegisteredRendererKind::Entry,
        &payload,
    )?;
    extension_text_component(&value)
}

/// Project an assistant message's content into the provider-free
/// [`AssistantBlock`] list (text, thinking, and decoded image blocks, in
/// document order) the `AssistantMessageComponent` renders. Tool-call blocks
/// are rendered by their own components in the transcript.
/// Whether startup intentionally opened a session that already has history.
pub(super) fn launch_restores_history(args: &Args) -> bool {
    args.continue_session
        || args.resume
        || args.session.is_some()
        || args.session_id.is_some()
        || args.fork.is_some()
}

pub(super) fn custom_message_fallback(custom: &rpi_agent::CustomMessage) -> String {
    let content = custom
        .content
        .iter()
        .filter_map(|item| match item {
            Content::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if content.is_empty() {
        format!("{}: {}", custom.role, custom.data)
    } else {
        format!("{}: {}", custom.role, content)
    }
}

/// Path to the enclosing repository's `.git/HEAD`, if any.
pub(super) fn find_git_head_path(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        let head = current.join(".git").join("HEAD");
        if head.exists() {
            return Some(head);
        }
        dir = current.parent();
    }
    None
}

/// Current git branch for `cwd`, or `None` outside a repository / on error.
/// Reads `.git/HEAD` directly (no `git` subprocess) so the footer can call it
/// cheaply at startup.
pub(super) fn git_branch_for(cwd: &std::path::Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        let head = current.join(".git").join("HEAD");
        if let Ok(contents) = std::fs::read_to_string(&head) {
            let trimmed = contents.trim();
            if let Some(reference) = trimmed.strip_prefix("ref: ") {
                let branch = reference
                    .rsplit('/')
                    .next()
                    .unwrap_or(reference)
                    .trim()
                    .to_string();
                if !branch.is_empty() {
                    return Some(branch);
                }
            }
            if !trimmed.is_empty() {
                return Some(trimmed.chars().take(7).collect());
            }
        }
        dir = current.parent();
    }
    None
}

/// The name displayed for a model id (last path segment / after the final
/// `:`), to keep the footer compact.
pub(super) fn short_model_name(id: &str) -> String {
    id.rsplit([':', '/'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(id)
        .to_string()
}
