//! B5a — the `RuntimeActionHost` impl over the harness, lived in `rpi-cli`
//! (NOT `rpi-harness`) so `rpi-extensions` stays a leaf in the crate DAG. The
//! trait is defined in `rpi-extensions` (JSON + primitives only); this is the
//! host side that bridges the 16 [`RuntimeActionId`] actions to the harness.
//!
//! 10 ops delegate to `harness.lane("main")` (the `AgentLane` surface backs
//! `prompt_message`/`prompt_text`/`get_active_tools`/`set_active_tools`/
//! `set_model`/`get_thinking_level`/`set_thinking_level`/`compact`/
//! `navigate_tree`). Run-ops serialize via `acquire_run`'s `active_run` guard —
//! a concurrent plugin invocation surfaces `lane_busy` as the harness error
//! string, the right behavior (a plugin can't re-enter an active run).
//!
//! 6 non-lane ops:
//! - `append_entry` → the harness's boundary-aware message writer, or raw
//!   extension data on the session; `set_session_name` → `set_name`.
//! - `get_system_prompt` → `AgentHarness::get_system_prompt` (B5a accessor).
//! - `new_session`/`fork`/`switch_session` → reuse `crate::session`'s
//!   create/fork/open helpers + `harness.set_session`.
//! - `reload` → handled by `ActionBridge.reload` (B5d); the host impl is the
//!   "not configured" fallback for bridges without a callback.
//!
//! ## Construction ordering (the load-bearing wrinkle)
//!
//! Extensions load **before** the harness is created (extensions provide the
//! tools the harness is built with), but the plugin stores the
//! `ActionBridge`'s `user_data` pointer during `register` and that pointer
//! must remain valid for the whole session. So the host cannot hold the
//! `AgentHarness` directly — it holds an [`Arc<OnceLock<WeakAgentHarness>>`]
//! that is **empty at register time** and **filled once** by
//! [`HarnessActionHost::set_harness`] immediately after `AgentHarness::create`
//! succeeds. Each action upgrades that weak binding for its duration; the host
//! must not keep tools/providers alive via adapter -> keepalive -> bridge -> host.
//! Before construction or after final harness release, actions fail explicitly.
//! The bridge's opaque handle stays stable without retaining the harness.
//!
//! The impl needs the model `catalog` for `set_model(id)` (the lane wants a
//! `Model`, not an id) and the `cwd` for session create/fork/switch. Both are
//! held by `rpi-cli` at build time and moved into the host.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use rpi_ai::types::ThinkingLevel;
use rpi_extensions::RuntimeActionHost;
use rpi_harness::agent_harness::{
    AgentHarness, HarnessRunOutcome, NavigationOutcome, SendMessageResult, WeakAgentHarness,
};
use rpi_harness::session::session::Session;
use rpi_harness::types::SendMessageOptions;
use tokio::runtime::Handle;

use crate::session::{default_session_dir, open_session_by_id_in_dir};

/// Merge built-in / extension / prompt-template / skill commands into the
/// `ListCommands` payload.
///
/// It is one function because it is the single source of truth for the shape:
///
/// * Built-in commands live in the TUI's `CommandRegistry`, not the plugin
///   registry. Reporting only the extension half leaves a remote client without
///   the most common entries (`/help`, `/model`, …).
/// * Prompt templates live in **neither**: the TUI merges them into autocomplete
///   separately (`build_builtin_registry` explicitly says so). They have to be
///   read from the harness resources, or a remote `/` completion list is
///   permanently shorter than the terminal's — same host, two command tables.
/// * Precedence is built-in > extension > prompt > skill, matching the TUI (built-ins
///   register first and cannot be shadowed; a template sharing a built-in name
///   is unreachable there too).
/// * Extension and template names may omit the leading `/`, so normalize.
///
/// Built-in **aliases and hidden commands** win dispatch in the TUI without
/// appearing as suggestions. Reserving them here keeps an extension that
/// registers the same name from being advertised as `source:"extension"` when
/// the built-in is what actually runs.
fn merge_command_list_reserved(
    builtin: Vec<(String, String)>,
    extension: Vec<(String, String)>,
    templates: Vec<(String, String)>,
    skills: Vec<(String, String)>,
    reserved: Vec<String>,
) -> Vec<serde_json::Value> {
    // Reserved names must block *later* sources without blocking the built-ins
    // themselves: a visible built-in canonical name is both the reservation and
    // a real entry, so seeding `seen` with it would drop every built-in from the
    // listing. Keep the reservation set separate and consult it only for
    // non-builtin sources; collisions among the sources still fall to `seen`.
    let mut reserved_names = std::collections::BTreeSet::new();
    for name in reserved {
        let name = if !name.starts_with('/') {
            format!("/{name}")
        } else {
            name
        };
        reserved_names.insert(name);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<serde_json::Value> = Vec::new();
    for (source, commands) in [
        ("builtin", builtin),
        ("extension", extension),
        ("prompt", templates),
        ("skill", skills),
    ] {
        for (name, description) in commands {
            let name = if source != "builtin" && !name.starts_with('/') {
                format!("/{name}")
            } else {
                name
            };
            if source != "builtin" && reserved_names.contains(&name) {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            out.push(serde_json::json!({
                "name": name,
                "description": description,
                "source": source,
            }));
        }
    }
    out
}

/// The `RuntimeActionHost` impl over an `AgentHarness`. Built once per session
/// in [`crate::session::build`] — constructed **empty** before extension load
/// (the harness doesn't exist yet), then filled via [`set_harness`](Self::set_harness)
/// once `AgentHarness::create` succeeds. Carried inside the [`ActionBridge`] as
/// `Arc<dyn RuntimeActionHost>`.
///
/// `runtime` is captured at build time so the bridge can spawn dispatch from
/// any thread; the impl's async methods run ON that runtime (they are spawned
/// by the trampoline), so they may freely await.
pub struct HarnessActionHost {
    /// Weak binding filled after construction. Each operation upgrades it;
    /// before construction or after final owner release it returns an error.
    harness: Arc<OnceLock<WeakAgentHarness>>,
    /// The auth-filtered catalog (the same list the TUI `/model` selector
    /// shows). `set_model(id)` resolves an id against this.
    catalog: RwLock<Vec<rpi_ai::Model>>,
    /// The session cwd — `new_session`/`fork`/`switch_session` need it to
    /// locate the session dir.
    cwd: PathBuf,
    session_dir: PathBuf,
    fresh_session_only: bool,
    /// Parsed unknown long flags retained for extension consumption.
    cli_flags: BTreeMap<String, serde_json::Value>,
    /// The session's plugin-tool context (`cwd` + session id). The session
    /// runtime actions (`new_session`/`fork`/`switch_session`) swap the session
    /// the harness serves, and must republish the id here too — a plugin whose
    /// tool state keyed off the old id would otherwise keep serving the session
    /// it named before the swap.
    tool_context: rpi_extensions::ToolCallContext,
    children: crate::child_agents::ChildAgents,
    side_questions: crate::side_questions::SideQuestions,
    #[allow(dead_code)]
    runtime: Handle,
    /// Loaded extension registry, set once the session's extensions are in.
    ///
    /// Needed by `ListCommands`: the plugin ABI had no way to enumerate slash
    /// commands, so a remote UI could not offer completion. `None` before the
    /// registry is published (and in tests that never load extensions) — the
    /// action then answers with an empty list rather than failing, matching
    /// the "no commands registered" case.
    registry: std::sync::RwLock<Option<std::sync::Arc<rpi_extensions::RegistrySnapshot>>>,
    /// Request channel for `runtime_action(ReloadModels)` (action id 31).
    ///
    /// The refresh itself must run on the TUI's main loop (it owns the
    /// harness), so the host hands the request over and awaits the answer.
    /// `None` until the TUI installs its sender.
    reload_models_mailbox: Option<rpi_extensions::ReloadModelsMailbox>,
}

impl HarnessActionHost {
    /// Build an **empty** host (no harness yet). The `runtime` is the handle
    /// the bridge captured (kept only so the impl can name it for future
    /// direct-spawn needs; the trampoline already spawns on the bridge's
    /// runtime). Call [`set_harness`](Self::set_harness) once the harness is
    /// created. Returns `(host, harness_cell)` where `harness_cell` is the
    /// shared `OnceLock` the caller fills.
    pub fn new_empty(
        catalog: Vec<rpi_ai::Model>,
        cwd: PathBuf,
        runtime: Handle,
        cli_flags: BTreeMap<String, serde_json::Value>,
        tool_context: rpi_extensions::ToolCallContext,
    ) -> (Self, Arc<OnceLock<WeakAgentHarness>>) {
        let harness = Arc::new(OnceLock::new());
        (
            Self {
                harness: Arc::clone(&harness),
                catalog: RwLock::new(catalog),
                session_dir: default_session_dir(&cwd),
                fresh_session_only: false,
                cwd,
                cli_flags,
                tool_context,
                children: crate::child_agents::ChildAgents::default(),
                side_questions: crate::side_questions::SideQuestions::default(),
                runtime,
                registry: std::sync::RwLock::new(None),
                reload_models_mailbox: None,
            },
            harness,
        )
    }

    /// Attach the `ReloadModels` request channel (once the TUI starts).
    pub(crate) fn with_reload_models_mailbox(
        mut self,
        mailbox: rpi_extensions::ReloadModelsMailbox,
    ) -> Self {
        self.reload_models_mailbox = Some(mailbox);
        self
    }

    pub fn set_catalog(&self, catalog: Vec<rpi_ai::Model>) {
        *self
            .catalog
            .write()
            .unwrap_or_else(|error| error.into_inner()) = catalog;
    }

    pub(crate) fn with_fresh_session_only(mut self, locked: bool) -> Self {
        self.fresh_session_only = locked;
        self
    }
    fn allow_session_routing(&self) -> Result<(), String> {
        if self.fresh_session_only {
            Err("experimental native print session forbids session routing actions".into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn with_session_dir(mut self, session_dir: PathBuf) -> Self {
        self.session_dir = session_dir;
        self
    }

    /// Publish the loaded extension registry so `ListCommands` can answer.
    ///
    /// `None` publishes "no extensions". A reload that ends with zero plugins
    /// must publish that too: otherwise the previous snapshot stays cached and
    /// removed extensions keep advertising their commands.
    pub(crate) fn publish_registry_impl(
        &self,
        snapshot: Option<std::sync::Arc<rpi_extensions::RegistrySnapshot>>,
    ) {
        if let Ok(mut slot) = self.registry.write() {
            *slot = snapshot;
        }
    }

    fn registry_snapshot(&self) -> Option<std::sync::Arc<rpi_extensions::RegistrySnapshot>> {
        self.registry.read().ok().and_then(|slot| slot.clone())
    }

    /// Fill the harness cell. Call exactly once, immediately after
    /// `AgentHarness::create` succeeds. Returns the host (for fluent chaining)
    /// — the caller already holds the `Arc<dyn RuntimeActionHost>` from
    /// construction; this just populates the cell that host reads.
    pub fn set_harness(cell: &Arc<OnceLock<WeakAgentHarness>>, harness: Arc<AgentHarness>) {
        let _ = cell.set(harness.downgrade());
    }

    /// Pin the harness only for this action. The binding itself must not retain
    /// tools/providers through the plugin -> keepalive -> host reference cycle.
    fn harness(&self) -> Result<AgentHarness, String> {
        self.harness
            .get()
            .ok_or_else(|| "runtime action invoked before harness was built".to_string())?
            .upgrade()
            .ok_or_else(|| "runtime action invoked after harness was dropped".to_string())
    }

    /// Resolve `id` (case-insensitive exact, then substring) against the
    /// catalog. Mirrors the resolver's exact-id-first fallback.
    fn resolve_model(&self, id: &str) -> Option<rpi_ai::Model> {
        let catalog = self.catalog.read().ok()?;
        catalog
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(id))
            .cloned()
            .or_else(|| {
                catalog
                    .iter()
                    .find(|m| m.id.to_ascii_lowercase().contains(&id.to_ascii_lowercase()))
                    .cloned()
            })
    }
}

/// Serialize reclaimed queue entries for the ABI boundary.
///
/// Each entry is `{"text": "…", "images": [{mediaType, data}]}` — text and its
/// own attachments **together**. A flat `images` side list would lose the
/// pairing as soon as two queued messages carry different pictures: a restore
/// would attach image B next to text A, or attach the picture to whichever
/// entry happened to be first.
pub(crate) fn queued_message_payloads(
    messages: &[rpi_agent::AgentMessage],
) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|message| {
            let (text, images) = match message {
                rpi_agent::AgentMessage::User(user) => match &user.content {
                    rpi_ai::types::UserContent::Text(text) => (text.clone(), Vec::new()),
                    rpi_ai::types::UserContent::Blocks(blocks) => {
                        let text = blocks
                            .iter()
                            .filter_map(|block| match block {
                                rpi_ai::types::Content::Text(text) => Some(text.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let collected: Vec<_> = blocks
                            .iter()
                            .filter_map(|block| match block {
                                rpi_ai::types::Content::Image(image) => Some(image.clone()),
                                _ => None,
                            })
                            .collect();
                        (text, image_payloads(&collected))
                    }
                },
                _ => (String::new(), Vec::new()),
            };
            serde_json::json!({ "text": text, "images": images })
        })
        .filter(|entry| {
            entry["text"].as_str().is_some_and(|t| !t.trim().is_empty())
                || entry["images"].as_array().is_some_and(|i| !i.is_empty())
        })
        .collect()
}

/// Serialize image attachments as `{mediaType, data}`.
fn image_payloads(images: &[rpi_ai::types::ImageContent]) -> Vec<serde_json::Value> {
    images
        .iter()
        .map(|image| {
            serde_json::json!({
                "mediaType": image.mime_type,
                "data": image.data,
            })
        })
        .collect()
}

/// Helper: pull a string field `key` from `args` (object), or return `msg`.
fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string field `{key}` in action args"))
}

/// Helper: pull an optional string field.
fn arg_str_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Helper: pull a bool field (default `false`).
fn arg_bool(args: &serde_json::Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Helper: pull a string array field.
fn arg_str_array(args: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .ok_or_else(|| format!("missing string-array field `{key}` in action args"))
}

/// Render a `HarnessRunOutcome` as JSON for the plugin. Only the terminal
/// status + leaf id cross (the final message text is folded to a string; full
/// assistant content is too rich for a v1 action result).
pub(crate) fn run_outcome_json(outcome: HarnessRunOutcome) -> serde_json::Value {
    match outcome {
        HarnessRunOutcome::Completed {
            leaf_id,
            final_entry_id,
            final_message,
        } => {
            serde_json::json!({
                "status": "completed",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
                "text": assistant_text(&final_message),
            })
        }
        HarnessRunOutcome::Aborted {
            leaf_id,
            final_entry_id,
            final_message,
        } => {
            serde_json::json!({
                "status": "aborted",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
                "text": assistant_text(&final_message),
            })
        }
        HarnessRunOutcome::Failed {
            leaf_id,
            error,
            final_entry_id,
            final_message,
        } => {
            serde_json::json!({
                "status": "failed",
                "leafId": leaf_id,
                "error": format!("{error:?}"),
                "finalEntryId": final_entry_id,
                "text": final_message.map(|m| assistant_text(&m)).unwrap_or_default(),
            })
        }
        HarnessRunOutcome::Suspended {
            leaf_id,
            final_entry_id,
            ..
        } => {
            serde_json::json!({
                "status": "suspended",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
            })
        }
    }
}

/// Accept the SDK's flat action args and Pi-style nested options. Reject mixed
/// forms instead of silently choosing one set of scheduling instructions.
fn send_message_options(args: &serde_json::Value) -> Result<SendMessageOptions, String> {
    let flat = args.get("triggerTurn").is_some() || args.get("deliverAs").is_some();
    let value = if let Some(options) = args.get("options").filter(|value| !value.is_null()) {
        if !options.is_object() {
            return Err("send_message options must be an object".into());
        }
        if flat {
            return Err(
                "put triggerTurn/deliverAs either in options or at the top level, not both".into(),
            );
        }
        options.clone()
    } else {
        let mut fields = serde_json::Map::new();
        for key in ["triggerTurn", "deliverAs"] {
            if let Some(value) = args.get(key) {
                fields.insert(key.into(), value.clone());
            }
        }
        serde_json::Value::Object(fields)
    };
    serde_json::from_value(value).map_err(|error| format!("invalid send_message options: {error}"))
}

fn send_message_input(args: &serde_json::Value) -> Result<rpi_agent::AgentMessage, String> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    if let Some(message) = args.get("message") {
        if message.get("kind").is_some() {
            return serde_json::from_value(message.clone())
                .map_err(|error| format!("invalid message: {error}"));
        }
        // Pi's custom message input does not include the stored AgentMessage
        // discriminator. Preserve customType/display/details via its constructor.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CustomInput {
            custom_type: String,
            #[serde(default)]
            content: Option<rpi_ai::types::UserContent>,
            #[serde(default)]
            display: bool,
            #[serde(default)]
            details: Option<serde_json::Value>,
        }
        let input: CustomInput = serde_json::from_value(message.clone())
            .map_err(|error| format!("invalid custom message: {error}"))?;
        return Ok(rpi_harness::messages::create_custom_message(
            &input.custom_type,
            input
                .content
                .unwrap_or(rpi_ai::types::UserContent::Blocks(Vec::new())),
            input.display,
            input.details,
            timestamp,
        ));
    }
    let text =
        arg_str(args, "text").map_err(|_| "missing `message` or `text` field".to_string())?;
    Ok(rpi_agent::AgentMessage::User(
        rpi_ai::types::UserMessage::new(text, timestamp),
    ))
}

fn send_message_result(result: SendMessageResult) -> serde_json::Value {
    match result {
        SendMessageResult::Appended { entry_id } => {
            serde_json::json!({"status":"appended", "entryId":entry_id})
        }
        SendMessageResult::Queued { entry_id, queue } => {
            if queue == rpi_harness::session::types::QueueKind::Context {
                return serde_json::json!({"status":"queued", "entryId":entry_id, "queue":"context"});
            }
            let delivery = match queue {
                rpi_harness::session::types::QueueKind::NextRun => "nextTurn",
                _ => queue.as_str(),
            };
            serde_json::json!({"status":"queued", "entryId":entry_id, "deliverAs":delivery})
        }
        SendMessageResult::Run(run) => run_outcome_json(run.outcome),
    }
}

/// Extract the concatenated text from an assistant message (the `text` blocks).
pub(crate) fn assistant_text(msg: &rpi_ai::types::AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            rpi_ai::types::Content::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[async_trait::async_trait]
impl RuntimeActionHost for HarnessActionHost {
    async fn tool_program(
        &self,
        capability: Arc<rpi_agent::tool_program::ToolProgram>,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Deliberately do not look up the root harness: capability owns the effective binding.
        capability.handle(args).await
    }
    fn cancel_side_questions(&self, owner: u64) {
        self.side_questions.cancel_owner(owner);
    }
    fn side_question_revision(&self) -> u64 {
        self.side_questions.revision()
    }
    fn side_question_views(&self) -> Vec<serde_json::Value> {
        self.side_questions.views()
    }
    async fn side_question_ui(
        &self,
        owner: u64,
        thread_id: String,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.side_questions
            .ui_handle(&self.harness()?, owner, thread_id, args)
            .await
    }
    async fn side_question(
        &self,
        owner: u64,
        args: serde_json::Value,
        sink: rpi_extensions::SideQuestionSink,
    ) -> Result<serde_json::Value, String> {
        self.side_questions
            .handle(&self.harness()?, owner, args, sink)
            .await
    }

    fn cancel_child_agents(&self, owner: u64) {
        self.children.cancel_owner(owner);
    }
    fn replace_model_catalog(&self, models: Vec<rpi_ai::Model>) {
        self.set_catalog(models);
    }
    async fn list_commands(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let builtin = crate::interactive_tui::build_builtin_commands_for_abi();
        let extension: Vec<(String, String)> = self
            .registry_snapshot()
            .as_deref()
            .map(|snapshot| {
                snapshot
                    .commands()
                    .iter()
                    .map(|c| (c.name.clone(), c.description.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // Read resources on every call so reloads cannot leave stale commands.
        // A missing/closed harness must still expose builtin/extension entries.
        // enableSkillCommands is TUI-only; RPC always enumerates loaded skills.
        let resources = match self.harness() {
            Ok(harness) => match harness.get_resources().await {
                Ok(resources) => crate::resource_commands::resource_command_entries(&resources),
                Err(_) => Default::default(),
            },
            Err(_) => Default::default(),
        };
        Ok(serde_json::json!({
            "commands": merge_command_list_reserved(
                builtin,
                extension,
                resources.templates,
                resources.skills,
                crate::interactive_tui::builtin_reserved_names(),
            )
        }))
    }

    fn publish_registry(&self, snapshot: std::sync::Arc<rpi_extensions::RegistrySnapshot>) {
        self.publish_registry_impl(Some(snapshot));
    }

    fn publish_registry_opt(
        &self,
        snapshot: Option<std::sync::Arc<rpi_extensions::RegistrySnapshot>>,
    ) {
        self.publish_registry_impl(snapshot);
    }

    async fn abort(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        // The harness reclaims the queue and cancels the run under one write
        // gate, so there is no `clear_queue()` + `abort()` window in which a
        // message could be both drained into the transcript and handed back.
        //
        // Why this matters: the agent loop drains the steering queue on abort
        // and emits those messages as transcript content. Native-pi's TUI
        // avoids the resulting "ghost message" by calling `clearQueue()` first
        // ("a cancel never silently swallows messages the user staged while the
        // run was in flight"). A plugin has no editor to restore into, so it
        // hands them back to its caller instead.
        // `abort_reclaiming(true)`: one call cancels **and** reclaims under the
        // harness's write gate, so the idle case is atomic too. A separate
        // `clear_queue()` afterwards would let a new run start and drain the
        // queue in between — the caller would then report "nothing was queued"
        // for messages that were in fact consumed.
        let harness = self.harness()?;
        let result = harness
            .abort_reclaiming(true)
            .await
            .map_err(|error| error.to_string())?;
        Ok(serde_json::json!({
            // Empty run id = the lane was already idle. The queue is still
            // handed back: a stop button must be idempotent, and messages can
            // land as a run settles.
            "aborted": !result.run_id.is_empty(),
            "runId": if result.run_id.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(result.run_id)
            },
            "steer": queued_message_payloads(&result.steer),
            "followUp": queued_message_payloads(&result.follow_up),
        }))
    }

    fn stop_round(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        self.harness()?.request_stop_round();
        Ok(serde_json::json!({ "requested": true }))
    }

    async fn get_execution_context(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let harness = self.harness()?;
        let mut context = harness.execution_context().map_err(|e| e.to_string())?;
        if let Some(serde_json::Value::Object(child)) =
            rpi_agent::tool_execution_scope::execution_context()
        {
            context
                .as_object_mut()
                .expect("execution context is an object")
                .extend(child);
        }
        context["cwd"] = serde_json::json!(self.cwd);
        context["sessionId"] = serde_json::json!(harness.session().storage().metadata().id);
        Ok(context)
    }

    async fn child_agent(
        &self,
        owner: u64,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if rpi_agent::tool_execution_scope::is_child_execution() {
            return Err("recursive child agents are not allowed".into());
        }
        if args.get("op").and_then(serde_json::Value::as_str) == Some("start") {
            self.children.handle(&self.harness()?, owner, args).await
        } else {
            // Reclamation must remain possible after the parent was dropped.
            self.children.handle_existing(owner, args)
        }
    }

    async fn get_cli_flag(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let name = arg_str(&args, "name")?;
        Ok(serde_json::json!({
            "value": self.cli_flags.get(&name).cloned().unwrap_or(serde_json::Value::Null)
        }))
    }

    async fn send_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let options = send_message_options(&args)?;
        let message = send_message_input(&args)?;
        let lane = self.harness()?.lane("main");
        let result = lane
            .send_message(message, options)
            .await
            .map_err(|e| e.to_string())?;
        Ok(send_message_result(result))
    }

    async fn send_user_message(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let text = arg_str(&args, "text")?;
        let lane = self.harness()?.lane("main");
        // Honor the same scheduling instructions `SendMessage` accepts.
        //
        // The action used to call `prompt_text` directly, which ignored
        // `deliverAs` entirely. Two consequences:
        //
        // 1. A message sent while a run is in flight was rejected with
        //    "lane busy" instead of being queued — and because this action
        //    blocks for the whole run, callers that fire-and-forget (a bridge,
        //    a remote UI) never saw the error. The message silently vanished.
        // 2. `steer` / `followUp` / `nextTurn` were all delivered as "start a
        //    new prompt", so a client could not express them at all.
        //
        // `send_message` with `triggerTurn: Some(true)` keeps the idle behavior
        // (start a run) while giving an active run the queueing semantics the
        // delivery mode asks for.
        let mut options = send_message_options(&args)?;
        if options.trigger_turn.is_none() {
            options.trigger_turn = Some(true);
        }
        let message = rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage::new(
            text,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(i64::MAX as u128) as i64,
        ));
        let result = lane
            .send_message(message, options)
            .await
            .map_err(|e| e.to_string())?;
        Ok(send_message_result(result))
    }

    async fn append_entry(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        // `{"message": <AgentMessage json>}` appends a message entry; OR
        // `{"customType": "...", "data": {...}}` appends a custom entry. No run
        // is driven. Messages use the harness boundary writer, whose entry id
        // may be reserved until the current turn settles. Custom data stays out
        // of model context unless a caller explicitly configures a projector.
        if let Some(expected) = args.get("expected") {
            let guard = serde_json::from_value(expected.clone())
                .map_err(|error| format!("invalid AppendEntry expected guard: {error}"))?;
            let entry = if let Some(custom_type) = arg_str_opt(&args, "customType") {
                if args.get("message").is_some() || args.get("outboxKey").is_some() {
                    return Err("scoped custom entry cannot include message/outboxKey".into());
                }
                crate::resource_selection::validate_public_entry(&custom_type, args.get("data"))?;
                rpi_harness::agent_harness::ScopedEntry::Custom {
                    custom_type,
                    data: args.get("data").cloned(),
                }
            } else {
                let message =
                    serde_json::from_value(args.get("message").cloned().ok_or("missing message")?)
                        .map_err(|error| format!("invalid message: {error}"))?;
                let key = arg_str(&args, "outboxKey")?;
                rpi_harness::agent_harness::ScopedEntry::Notice { message, key }
            };
            let id = self
                .harness()?
                .append_scoped_entry(&guard, entry)
                .await
                .map_err(|error| error.to_string())?;
            return Ok(serde_json::json!({"entryId":id, "scoped":true}));
        }
        if args.get("outboxKey").is_some() {
            return Err("outboxKey requires an expected guard".into());
        }
        if let Some(custom_type) = arg_str_opt(&args, "customType") {
            crate::resource_selection::validate_public_entry(&custom_type, args.get("data"))?;
            let data = args.get("data").cloned();
            let id = self
                .harness()?
                .append_custom_entry(&custom_type, data)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(serde_json::json!({ "entryId": id }));
        }
        let msg = args
            .get("message")
            .ok_or_else(|| "missing `message` or `customType` field".to_string())?;
        let message: rpi_agent::AgentMessage =
            serde_json::from_value(msg.clone()).map_err(|e| format!("invalid message: {e}"))?;
        let id = self
            .harness()?
            .append_message(message)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "entryId": id }))
    }

    async fn get_session_branch(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let request = crate::modes::branch_entry_query(&args)?;
        let (entries, session_id, leaf_id) =
            crate::modes::branch_snapshot(&self.harness()?, &request).await?;
        let entries = entries
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("serialize session branch: {e}"))?;
        Ok(serde_json::json!({
            "entries": entries,
            "count": entries.len(),
            "filtered": true,
            "sessionId": session_id,
            "leafId": leaf_id,
            "scopedAppend": true,
        }))
    }

    async fn set_session_name(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let name = arg_str(&args, "name")?;
        self.harness()?
            .session()
            .set_name(Some(&name))
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn get_active_tools(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let lane = self.harness()?.lane("main");
        let tools = lane.get_active_tools().await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "tools": tools }))
    }

    async fn set_active_tools(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let tools = arg_str_array(&args, "tools")?;
        let lane = self.harness()?.lane("main");
        lane.set_active_tools(tools)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn set_model(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = arg_str(&args, "model")?;
        let model = self
            .resolve_model(&id)
            .ok_or_else(|| format!("model `{id}` not in catalog"))?;
        let lane = self.harness()?.lane("main");
        lane.set_model(model.clone())
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "model": model.id }))
    }

    async fn get_thinking_level(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let lane = self.harness()?.lane("main");
        let level = lane.get_thinking_level().await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "level": level }))
    }

    async fn set_thinking_level(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let level_val = args
            .get("level")
            .ok_or_else(|| "missing `level` field".to_string())?;
        let level: ThinkingLevel = if let Some(s) = level_val.as_str() {
            serde_json::from_value(serde_json::Value::String(s.to_string()))
                .map_err(|e| format!("invalid thinking level `{s}`: {e}"))?
        } else {
            serde_json::from_value(level_val.clone())
                .map_err(|e| format!("invalid thinking level: {e}"))?
        };
        let lane = self.harness()?.lane("main");
        lane.set_thinking_level(level)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn compact(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let custom = arg_str_opt(&args, "customInstructions");
        let lane = self.harness()?.lane("main");
        let result = lane
            .compact(custom.as_deref())
            .await
            .map_err(|e| e.to_string())?;
        Ok(
            serde_json::json!({ "runId": result.run_id, "outcome": format!("{:?}", result.outcome) }),
        )
    }

    async fn get_system_prompt(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let prompt = self
            .harness()?
            .get_system_prompt()
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "prompt": prompt }))
    }

    async fn new_session(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        self.allow_session_routing()?;
        let harness = self.harness()?;
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let dir = &self.session_dir;
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create session dir {}: {e}", dir.display()))?;
        let session = crate::session::create_jsonl_session(dir, &cwd_str)
            .await
            .map_err(|e| format!("create session: {e}"))?;
        let id = session.storage().metadata().id.clone();
        crate::session::set_active_session(&harness, session, Some(&self.tool_context)).await?;
        Ok(serde_json::json!({ "sessionId": id }))
    }

    async fn fork(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        self.allow_session_routing()?;
        let harness = self.harness()?;
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let new_session =
            crate::session::fork_session_storage(&harness, &cwd_str, &self.session_dir)
                .await
                .map_err(|e| format!("fork session: {e}"))?;
        let id = new_session.storage().metadata().id.clone();
        crate::session::set_active_session(&harness, new_session, Some(&self.tool_context)).await?;
        Ok(serde_json::json!({ "sessionId": id }))
    }

    async fn navigate_tree(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        self.allow_session_routing()?;
        let target_id = arg_str_opt(&args, "targetId");
        let summarize = arg_bool(&args, "summarize");
        let custom = arg_str_opt(&args, "customInstructions");
        let label = arg_str_opt(&args, "label");
        let lane = self.harness()?.lane("main");
        let result = lane
            .navigate_tree(
                target_id.as_deref(),
                summarize,
                custom.as_deref(),
                label.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let status = match &result.outcome {
            NavigationOutcome::Completed { .. } => "completed",
            NavigationOutcome::Declined { .. } => "declined",
            NavigationOutcome::Aborted { .. } => "aborted",
            NavigationOutcome::Failed { .. } => "failed",
        };
        Ok(serde_json::json!({ "runId": result.run_id, "status": status }))
    }

    async fn switch_session(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        self.allow_session_routing()?;
        let harness = self.harness()?;
        let id = arg_str(&args, "id")?;
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let new_session: Session = open_session_by_id_in_dir(&id, &cwd_str, &self.session_dir)
            .await
            .map(|restored| restored.session)
            .map_err(|e| e.to_string())?;
        let new_id = new_session.storage().metadata().id.clone();
        crate::session::set_active_session(&harness, new_session, Some(&self.tool_context)).await?;
        Ok(serde_json::json!({ "sessionId": new_id }))
    }

    async fn reload(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        // Reached only when `ActionBridge.reload` is `None` (no /reload wired).
        // B5d wires the reload callback at the bridge layer; this host impl is
        // the "not configured" fallback.
        Err("reload not configured (no /reload callback on this bridge)".to_string())
    }

    /// `ReloadModels` — re-read the model/provider catalog from disk without
    /// touching the extension registry.
    ///
    /// The work happens on the TUI's main loop (it owns the harness), so the
    /// request goes through the `ReloadModelsMailbox` and the answer comes
    /// back over the same request's reply channel.
    async fn reload_models(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let Some(mailbox) = self.reload_models_mailbox.clone() else {
            return Err("reload_models is not available (no TUI attached)".to_string());
        };
        let count = mailbox.request().await?;
        Ok(serde_json::json!({ "models": count }))
    }
}

#[cfg(test)]
mod lifetime_tests;

#[cfg(test)]
mod send_message_tests {
    use super::*;
    use rpi_harness::types::MessageDelivery;

    #[test]
    fn options_accept_flat_and_nested_forms_with_pi_defaults() {
        assert_eq!(
            send_message_options(&serde_json::json!({"text":"context"})).unwrap(),
            SendMessageOptions::default()
        );
        let expected = SendMessageOptions {
            trigger_turn: Some(false),
            deliver_as: MessageDelivery::FollowUp,
        };
        for args in [
            serde_json::json!({"text":"context", "triggerTurn":false, "deliverAs":"followUp"}),
            serde_json::json!({"text":"context", "options":{"triggerTurn":false, "deliverAs":"followUp"}}),
        ] {
            assert_eq!(send_message_options(&args).unwrap(), expected);
        }
    }

    #[test]
    fn options_reject_invalid_or_ambiguous_scheduling_instructions() {
        for args in [
            serde_json::json!({"triggerTurn":"false"}),
            serde_json::json!({"deliverAs":"follow-up"}),
            serde_json::json!({"options":{"typo":true}}),
            serde_json::json!({"options":[], "text":"context"}),
            serde_json::json!({"options":{"triggerTurn":true}, "triggerTurn":false}),
        ] {
            assert!(send_message_options(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn pi_custom_input_preserves_content_and_display_metadata() {
        let message = send_message_input(&serde_json::json!({"message":{
            "customType":"goal", "content":"goal state", "display":true, "details":{"step":2}
        }}))
        .unwrap();
        let rpi_agent::AgentMessage::Custom(custom) = &message else {
            panic!("custom")
        };
        assert_eq!(
            custom.content,
            vec![rpi_ai::types::Content::text("goal state")]
        );
        let data = rpi_harness::messages::custom_data(&message).unwrap();
        assert_eq!(data.custom_type, "goal");
        assert!(data.display);
        assert_eq!(data.details, Some(serde_json::json!({"step":2})));
        // The existing stored AgentMessage input remains supported.
        let stored = serde_json::to_value(&message).unwrap();
        assert_eq!(
            send_message_input(&serde_json::json!({"message":stored})).unwrap(),
            message
        );
    }

    #[test]
    fn missing_custom_content_is_empty_and_queued_results_use_pi_delivery_names() {
        let message = send_message_input(
            &serde_json::json!({"message":{"customType":"goal", "content":null}}),
        )
        .unwrap();
        assert!(
            matches!(message, rpi_agent::AgentMessage::Custom(custom) if custom.content.is_empty())
        );
        assert_eq!(
            send_message_result(SendMessageResult::Queued {
                entry_id: "reserved".into(),
                queue: rpi_harness::session::types::QueueKind::NextRun,
            }),
            serde_json::json!({"status":"queued", "entryId":"reserved", "deliverAs":"nextTurn"})
        );
    }
}

#[cfg(test)]
mod list_commands_tests {
    use super::*;
    use rpi_harness::types::{AgentHarnessOptions, AgentHarnessResources, PromptTemplate, Skill};

    #[tokio::test]
    async fn review_list_commands_must_include_pi_style_skill_names() {
        let root = tempfile::tempdir().unwrap();
        let harness = Arc::new(
            AgentHarness::create(AgentHarnessOptions {
                resources: AgentHarnessResources {
                    skills: Some(vec![Skill {
                        name: "manual".into(),
                        description: "Explicit only".into(),
                        content: "body".into(),
                        file_path: "/skills/manual/SKILL.md".into(),
                        disable_model_invocation: Some(true),
                    }]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let (host, cell) = HarnessActionHost::new_empty(
            vec![],
            root.path().to_path_buf(),
            Handle::current(),
            Default::default(),
            rpi_extensions::ToolCallContext::new(root.path().to_string_lossy()),
        );
        HarnessActionHost::set_harness(&cell, harness.clone());
        let listing = host.list_commands(serde_json::json!({})).await.unwrap();
        let command = listing["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "/skill:manual")
            .expect("loaded explicit-only skill must be discoverable");
        assert_eq!(command["source"], "skill");
        assert_eq!(command["description"], "Explicit only");
    }

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            content: "body".into(),
            file_path: format!("/skills/{name}/SKILL.md"),
            disable_model_invocation: Some(false),
        }
    }

    fn template(name: &str) -> PromptTemplate {
        PromptTemplate {
            name: name.into(),
            description: None,
            content: "body".into(),
        }
    }

    fn host(root: &std::path::Path) -> (HarnessActionHost, Arc<OnceLock<WeakAgentHarness>>) {
        let (host, cell) = HarnessActionHost::new_empty(
            vec![],
            root.to_path_buf(),
            Handle::current(),
            Default::default(),
            rpi_extensions::ToolCallContext::new(root.to_string_lossy()),
        );
        extern "C" fn unused_command(
            _: rpi_plugin_sdk::StbStringRef,
            _: *mut rpi_plugin_sdk::StbString,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            0
        }
        let mut registry = rpi_extensions::ExtensionRegistry::new();
        for name in ["deploy", "help", "q", "skill:extension"] {
            registry.register_command(
                name.into(),
                "Extension command".into(),
                unused_command,
                std::ptr::null_mut(),
            );
        }
        host.publish_registry(Arc::new(registry.snapshot()));
        (host, cell)
    }

    #[tokio::test]
    async fn list_commands_merges_sources_deduplicates_and_preserves_priority() {
        let root = tempfile::tempdir().unwrap();
        let harness = Arc::new(
            AgentHarness::create(AgentHarnessOptions {
                resources: AgentHarnessResources {
                    skills: Some(vec![
                        skill("extension", "shadowed"),
                        skill("prompt", "shadowed"),
                        skill("manual", "First skill"),
                        skill("manual", "Duplicate skill"),
                    ]),
                    prompt_templates: Some(vec![
                        template("help"),
                        template("q"),
                        template("deploy"),
                        template("skill:prompt"),
                        template("summarize"),
                        template("/summarize"),
                    ]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let (host, cell) = host(root.path());
        HarnessActionHost::set_harness(&cell, harness.clone());
        let listing = host.list_commands(serde_json::json!({})).await.unwrap();
        let commands = listing["commands"].as_array().unwrap();
        let find = |name| commands.iter().find(|c| c["name"] == name).unwrap();
        assert_eq!(find("/help")["source"], "builtin");
        assert_eq!(find("/deploy")["source"], "extension");
        assert_eq!(find("/skill:extension")["source"], "extension");
        assert_eq!(find("/skill:prompt")["source"], "prompt");
        assert_eq!(find("/summarize")["description"], "Expand prompt template");
        assert_eq!(find("/skill:manual")["source"], "skill");
        assert_eq!(find("/skill:manual")["description"], "First skill");
        assert!(!names(commands).contains(&"/q"));
        let unique: std::collections::BTreeSet<_> = names(commands).into_iter().collect();
        assert_eq!(unique.len(), commands.len());
        assert_eq!(commands.last().unwrap()["name"], "/skill:manual");
    }

    #[tokio::test]
    async fn list_commands_reads_reloaded_resources_and_removes_stale_skills() {
        let root = tempfile::tempdir().unwrap();
        let harness = Arc::new(AgentHarness::create(Default::default()).await.unwrap());
        let (host, cell) = host(root.path());
        HarnessActionHost::set_harness(&cell, harness.clone());
        for name in ["oldskill", "freshskill"] {
            harness
                .set_resources(AgentHarnessResources {
                    skills: Some(vec![skill(name, "Reloaded skill")]),
                    ..Default::default()
                })
                .await
                .unwrap();
            let listing = host.list_commands(serde_json::json!({})).await.unwrap();
            let commands = listing["commands"].as_array().unwrap();
            assert!(names(commands).contains(&format!("/skill:{name}").as_str()));
            if name == "freshskill" {
                assert!(!names(commands).contains(&"/skill:oldskill"));
            }
        }
        harness.set_resources(Default::default()).await.unwrap();
        let listing = host.list_commands(serde_json::json!({})).await.unwrap();
        assert!(!names(listing["commands"].as_array().unwrap()).contains(&"/skill:freshskill"));
    }

    #[tokio::test]
    async fn list_commands_without_live_harness_keeps_builtin_and_extension_entries() {
        let root = tempfile::tempdir().unwrap();
        let (host, cell) = host(root.path());
        let unbound = host.list_commands(serde_json::json!({})).await.unwrap();
        let commands = unbound["commands"].as_array().unwrap();
        assert!(commands
            .iter()
            .any(|c| c["name"] == "/help" && c["source"] == "builtin"));
        assert!(commands
            .iter()
            .any(|c| c["name"] == "/deploy" && c["source"] == "extension"));
        let harness = Arc::new(
            AgentHarness::create(AgentHarnessOptions {
                resources: AgentHarnessResources {
                    skills: Some(vec![skill("manual", "Manual")]),
                    prompt_templates: Some(vec![template("summarize")]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        HarnessActionHost::set_harness(&cell, harness.clone());
        harness.close().await.unwrap();
        assert_eq!(
            host.list_commands(serde_json::json!({})).await.unwrap(),
            unbound
        );
        drop(harness);
        assert_eq!(
            host.list_commands(serde_json::json!({})).await.unwrap(),
            unbound
        );
    }

    /// Reserving names must not delete the built-ins from the listing.
    ///
    /// The reservation set contains every built-in canonical name, so seeding
    /// the dedupe set with it silently dropped all of them: the real listing
    /// went from 32 built-ins to 0, leaving only extension entries.
    #[test]
    fn reserved_names_do_not_remove_the_builtins_themselves() {
        let builtin = vec![
            ("/help".to_string(), "help".to_string()),
            ("/model".to_string(), "model".to_string()),
        ];
        let out = merge_command_list_reserved(
            builtin,
            vec![("deploy".into(), "deploy".into())],
            vec![],
            vec![],
            // Every built-in name is reserved (as in production), plus an alias
            // and a hidden command that are not listed.
            vec![
                "/help".to_string(),
                "/model".to_string(),
                "/q".to_string(),
                "/context".to_string(),
            ],
        );
        assert_eq!(names(&out), vec!["/help", "/model", "/deploy"]);
        assert_eq!(out[0]["source"], "builtin");
        assert_eq!(out[1]["source"], "builtin");
        assert_eq!(out[2]["source"], "extension");
    }

    /// A built-in alias or hidden built-in wins TUI dispatch without showing up
    /// as a suggestion, so an extension registering the same name must not be
    /// advertised as `source:"extension"`.
    #[test]
    fn reserved_builtin_aliases_hide_colliding_extension_entries() {
        let out = merge_command_list_reserved(
            vec![("/help".into(), "help".into())],
            vec![
                ("q".into(), "quit".into()),
                ("context".into(), "context".into()),
                ("deploy".into(), "deploy".into()),
            ],
            vec![],
            vec![],
            vec!["/q".into(), "/context".into()],
        );
        // `/q` and `/context` are unreachable, `/deploy` is not a built-in.
        assert_eq!(names(&out), vec!["/help", "/deploy"]);
        assert_eq!(out[1]["source"], "extension");
    }

    fn names(v: &[serde_json::Value]) -> Vec<&str> {
        v.iter().map(|c| c["name"].as_str().unwrap()).collect()
    }

    #[test]
    fn builtin_wins_over_a_same_named_extension_command() {
        let out = merge_command_list_reserved(
            vec![("/model".into(), "built-in".into())],
            vec![("/model".into(), "extension".into())],
            vec![],
            vec![],
            Vec::new(),
        );
        assert_eq!(names(&out), vec!["/model"]);
        assert_eq!(out[0]["source"], "builtin");
        assert_eq!(out[0]["description"], "built-in");
    }

    #[test]
    fn extension_and_template_names_get_a_leading_slash() {
        let out = merge_command_list_reserved(
            vec![],
            vec![("deploy".into(), "deploy".into())],
            vec![("review".into(), "review".into())],
            vec![],
            Vec::new(),
        );
        assert_eq!(names(&out), vec!["/deploy", "/review"]);
        assert_eq!(out[0]["source"], "extension");
        assert_eq!(out[1]["source"], "prompt");
    }

    /// Prompt templates live in neither the built-in registry nor the extension
    /// registry, so `ListCommands` has to merge them itself — otherwise a remote
    /// `/` completion list is permanently shorter than the terminal's.
    #[test]
    fn prompt_templates_are_listed_with_their_own_source() {
        let out = merge_command_list_reserved(
            vec![("/help".into(), "help".into())],
            vec![],
            vec![("review".into(), "review a diff".into())],
            vec![],
            Vec::new(),
        );
        assert_eq!(names(&out), vec!["/help", "/review"]);
        assert_eq!(out[1]["source"], "prompt");
        assert_eq!(out[1]["description"], "review a diff");
    }

    /// A template sharing a built-in name is unreachable in the TUI too, so it
    /// must not be reported twice (or as `prompt`).
    #[test]
    fn a_builtin_name_is_not_reported_as_a_template() {
        let out = merge_command_list_reserved(
            vec![("/review".into(), "built-in".into())],
            vec![],
            vec![("review".into(), "template".into())],
            vec![],
            Vec::new(),
        );
        assert_eq!(names(&out), vec!["/review"]);
        assert_eq!(out[0]["source"], "builtin");
    }

    #[test]
    fn no_commands_is_an_empty_list_not_an_error() {
        assert!(merge_command_list_reserved(vec![], vec![], vec![], vec![], Vec::new()).is_empty());
    }
}
