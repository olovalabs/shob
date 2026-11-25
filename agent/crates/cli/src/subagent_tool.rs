//! Built-in `subagent` tool backed by `AgentHarness::create_child`.
//!
//! Runs bounded, isolated child sessions using the parent harness's active
//! model, provider/auth bindings, thinking level, cwd, and permission hooks.
//! Child sessions cannot recursively spawn subagents or expand capabilities.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use agent::{AgentEmitter, AgentError, AgentEvent, AgentTool};
use ai::types::{AssistantMessageEvent, Schema, StopReason, Tool};
use async_trait::async_trait;
use futures::FutureExt;
use harness::agent_harness::{AgentHarness, HarnessRunOutcome, RunResult, WeakAgentHarness};
use harness::result::HarnessError;
use harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use harness::session::memory::{InMemorySessionStorage, SystemClock};
use harness::session::session::DefaultIdGenerator;
use harness::session::types::{SessionMetadata, SessionStorage};
use harness::session::{Clock, Session};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tools::OsExecutionEnv;

use crate::extensions_actions::assistant_text;

pub const NAME: &str = "subagent";

const DEFAULT_MAX_TURNS: u32 = 12;
const MAX_TURNS: u32 = 32;
const DEFAULT_TIMEOUT_SECONDS: u64 = 600;
const MAX_ACTIVE_SUBAGENTS: usize = 16;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_REPORT_BYTES: usize = 64 * 1024;
const MAX_PREVIEW_BYTES: usize = 2048;
const UPDATE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn prompt_metadata() -> extensions::ToolPromptMetadata {
    extensions::ToolPromptMetadata {
        prompt_snippet: Some(
            "Spawn a focused subagent to investigate, analyze, or execute an isolated subtask"
                .into(),
        ),
        prompt_guidelines: vec![
            "Use subagent when a task benefits from isolated context or parallel exploration. Issue independent calls in the same response to run them concurrently. Give each worker a clear deliverable. The tool returns its report and, when available, a transcript path you can read in sections for more detail."
                .into(),
        ],
    }
}

pub(crate) struct SubagentTool {
    schema: Tool,
    harness_cell: Arc<OnceLock<WeakAgentHarness>>,
    cwd: PathBuf,
    session_dir: Option<PathBuf>,
    active: Arc<AtomicUsize>,
}

struct ActivePermit(Arc<AtomicUsize>);

impl Drop for ActivePermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl SubagentTool {
    pub(crate) fn create(
        harness_cell: Arc<OnceLock<WeakAgentHarness>>,
        cwd: PathBuf,
        session_dir: Option<PathBuf>,
    ) -> Arc<dyn AgentTool> {
        Arc::new(Self {
            schema: Tool {
                name: NAME.into(),
                description: "Run a focused subagent in an isolated context window for research, code exploration, review, or a self-contained subtask. Multiple subagent calls in the same turn execute in parallel. Subagents inherit the active model, working directory, and approval rules, and cannot spawn nested subagents.".into(),
                parameters: Schema::new(json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 20000,
                            "description": "Clear, self-contained task instructions and expected deliverable for the subagent."
                        },
                        "role": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 120,
                            "description": "Short display label or role (e.g. 'Researcher', 'Explorer', 'Reviewer'). Defaults to 'Worker'."
                        },
                        "tools": {
                            "type": "array",
                            "items": { "type": "string", "minLength": 1 },
                            "description": "Optional allowlist of tool names (subset of active parent tools). Omit to inherit all non-delegation active tools."
                        },
                        "maxTurns": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_TURNS,
                            "description": "Maximum turns for the subagent (1-32, default 12)."
                        }
                    },
                    "required": ["task"],
                    "additionalProperties": false
                })),
                constrained_sampling: None,
            },
            harness_cell,
            cwd,
            session_dir,
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn acquire_permit(&self) -> Result<ActivePermit, AgentError> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= MAX_ACTIVE_SUBAGENTS {
                return Err(AgentError::Tool(
                    "too many concurrent subagents active; wait for running subagents to finish"
                        .into(),
                ));
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(ActivePermit(Arc::clone(&self.active))),
                Err(observed) => current = observed,
            }
        }
    }

    async fn create_child_session(
        &self,
        parent: &AgentHarness,
        child_id: &str,
    ) -> (Session, Option<PathBuf>) {
        let parent_id = parent.session().storage().metadata().id.clone();
        if let Some(root) = &self.session_dir {
            let safe_parent = sanitize_id(&parent_id, "parent");
            let safe_child = sanitize_id(child_id, "child");
            let dir = root.join("subagents").join(safe_parent);
            let path = dir.join(format!("{safe_child}.jsonl"));
            let env = Arc::new(OsExecutionEnv::with_cwd(self.cwd.clone()));
            let fs: Arc<dyn tools::FileSystem> = env;
            if fs
                .create_dir(&dir.to_string_lossy(), true, None)
                .await
                .is_ok()
            {
                let clock: Arc<dyn Clock> = Arc::new(SystemClock);
                let header = JsonlV4Header::new(
                    child_id.to_owned(),
                    clock.now_ms(),
                    self.cwd.to_string_lossy().into_owned(),
                    Some(parent_id.clone()),
                    None,
                    None,
                );
                if let Ok(storage) = JsonlSessionStorage::create(
                    fs,
                    &path.to_string_lossy(),
                    header,
                    clock,
                    Arc::new(DefaultIdGenerator::new()),
                )
                .await
                {
                    let storage_arc: Arc<dyn SessionStorage> = Arc::new(storage);
                    return (Session::new(storage_arc, None), Some(path));
                }
            }
        }

        let session = Session::new(
            Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: child_id.to_owned(),
                    created_at: 0,
                    parent_session_id: Some(parent_id),
                },
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )),
            None,
        );
        (session, None)
    }
}

#[async_trait]
impl AgentTool for SubagentTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "Subagent"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn prepare_arguments(&self, args: Value) -> Result<Value, AgentError> {
        let Value::Object(mut map) = args else {
            return Ok(args);
        };
        if !map.contains_key("task") {
            for alias in ["prompt", "description", "instructions", "assignment"] {
                if let Some(value) = map.remove(alias) {
                    if let Some(text) = value.as_str().filter(|s| !s.trim().is_empty()) {
                        map.insert("task".into(), Value::String(text.to_owned()));
                        break;
                    }
                }
            }
        }
        if !map.contains_key("role") {
            for alias in ["label", "agent", "subagent_type", "subagentType", "title"] {
                if let Some(value) = map.remove(alias) {
                    if let Some(text) = value.as_str().filter(|s| !s.trim().is_empty()) {
                        map.insert("role".into(), Value::String(text.to_owned()));
                        break;
                    }
                }
            }
        }
        if !map.contains_key("maxTurns") {
            if let Some(value) = map.remove("max_turns") {
                map.insert("maxTurns".into(), value);
            }
        }
        Ok(Value::Object(map))
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if agent::tool_execution_scope::is_child_execution() {
            return Err(AgentError::Tool("nested subagents are not allowed".into()));
        }
        if signal.is_cancelled() {
            return Err(AgentError::Tool("subagent cancelled".into()));
        }
        let invocation_signal = agent::tool_execution_scope::invocation_signal();
        if invocation_signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(AgentError::Tool("parent tool invocation cancelled".into()));
        }

        let task = params
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AgentError::Validation("task must not be empty".into()))?
            .to_owned();
        if task.len() > 20_000 {
            return Err(AgentError::Validation(
                "task must be at most 20000 bytes".into(),
            ));
        }

        let role = params
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Worker")
            .to_owned();
        if role.len() > 120 {
            return Err(AgentError::Validation(
                "role must be at most 120 bytes".into(),
            ));
        }

        let max_turns = match params.get("maxTurns") {
            None => DEFAULT_MAX_TURNS,
            Some(value) => {
                let raw = value
                    .as_u64()
                    .ok_or_else(|| AgentError::Validation("maxTurns must be an integer".into()))?;
                if !(1..=MAX_TURNS as u64).contains(&raw) {
                    return Err(AgentError::Validation(format!(
                        "maxTurns must be between 1 and {MAX_TURNS}"
                    )));
                }
                raw as u32
            }
        };

        let requested_tools = params
            .get("tools")
            .map(|value| {
                value
                    .as_array()
                    .ok_or_else(|| AgentError::Validation("tools must be a string array".into()))
                    .and_then(|values| {
                        values
                            .iter()
                            .map(|entry| {
                                entry
                                    .as_str()
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_owned)
                                    .ok_or_else(|| {
                                        AgentError::Validation(
                                            "tools must contain non-empty strings".into(),
                                        )
                                    })
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
            })
            .transpose()?;

        let _permit = self.acquire_permit()?;

        let parent = self
            .harness_cell
            .get()
            .and_then(WeakAgentHarness::upgrade)
            .ok_or_else(|| AgentError::Tool("parent agent harness is not active".into()))?;

        let child_id = parent.session().id_generator().next();
        let (session, transcript_path) = self.create_child_session(&parent, &child_id).await;

        let observer = Arc::new(ChildObserver::new(
            child_id.clone(),
            role.clone(),
            transcript_path.clone(),
            on_update.clone(),
        ));
        let child = parent
            .create_child(
                session,
                Arc::new(ChildEmitter(Arc::clone(&observer))),
                max_turns,
                requested_tools,
            )
            .await;
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                return finish_result(
                    &child_id,
                    &role,
                    "failed",
                    transcript_path.as_deref(),
                    format!("Could not start subagent: {error}"),
                )
            }
        };

        // Emit initial running worker snapshot so the host UI immediately renders
        // the subagent row and binds its live transcript viewer.
        on_update(ToolResultPartial {
            content: vec![TextContentOrImage::text(format!("Starting {role}…"))],
            details: progress_details(
                &child_id,
                &role,
                "running",
                transcript_path.as_deref(),
                None,
            ),
            ..Default::default()
        });

        let prompt = format!(
            "You are a delegated {role} subagent. Complete the task in at most {max_turns} turns. Do not attempt to spawn subagents or delegate work. Work only in the current project and return a concise, verified final report.\n\nTask:\n{task}"
        );

        let execution_scope = Arc::clone(&child.execution_scope);
        let cleanup_harness = child.harness.clone();
        let outcome = std::panic::AssertUnwindSafe(run_child(
            child,
            observer,
            prompt,
            signal,
            invocation_signal,
            transcript_path.as_deref(),
            &child_id,
            &role,
        ))
        .catch_unwind()
        .await;

        let result = match outcome {
            Ok(result) => result,
            Err(_) => {
                let _ = cleanup_harness.close().await;
                finish_result(
                    &child_id,
                    &role,
                    "failed",
                    transcript_path.as_deref(),
                    "Subagent runner panicked.".into(),
                )
            }
        };
        execution_scope.wait_idle().await;
        result
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_child(
    child: harness::agent_harness::ChildHarness,
    observer: Arc<ChildObserver>,
    prompt: String,
    signal: CancellationToken,
    invocation_signal: Option<CancellationToken>,
    transcript_path: Option<&Path>,
    child_id: &str,
    role: &str,
) -> Result<AgentToolResult, AgentError> {
    let lane = child.harness.lane("main");
    let run = lane.prompt_text(&prompt, Vec::new());
    tokio::pin!(run);

    let parent_abort = async {
        match &child.parent_signal {
            Some(sig) => sig.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    let invocation_abort = async {
        match &invocation_signal {
            Some(sig) => sig.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };

    enum StopReasonKind {
        Finished(Result<RunResult, HarnessError>),
        Cancelled(&'static str),
        TimedOut,
        OutputLimit,
    }

    let stop = tokio::select! {
        biased;
        _ = observer.limit_exceeded.cancelled() => StopReasonKind::OutputLimit,
        _ = signal.cancelled() => StopReasonKind::Cancelled("subagent cancelled"),
        _ = invocation_abort => StopReasonKind::Cancelled("parent tool invocation cancelled"),
        _ = child.scope.cancelled() => StopReasonKind::Cancelled("parent run ended"),
        _ = parent_abort => StopReasonKind::Cancelled("parent run cancelled"),
        _ = tokio::time::sleep(Duration::from_secs(DEFAULT_TIMEOUT_SECONDS)) => StopReasonKind::TimedOut,
        _ = observer.publish_updates() => unreachable!(),
        result = &mut run => StopReasonKind::Finished(result),
    };

    let (status, notice, text) = match stop {
        StopReasonKind::Finished(Ok(result)) => {
            let _ = child.harness.drain_session().await;
            let _ = child.harness.close().await;
            match result.outcome {
                HarnessRunOutcome::Completed { final_message, .. } => {
                    let text = assistant_text(&final_message);
                    match final_message.stop_reason {
                        StopReason::ToolUse => (
                            "failed",
                            Some("Subagent stopped after using tools without a final report. It may have reached its turn limit.".into()),
                            text,
                        ),
                        StopReason::Length => (
                            "failed",
                            Some("Subagent reached the model's output limit; the report is incomplete.".into()),
                            text,
                        ),
                        _ if text.trim().is_empty() => (
                            "failed",
                            Some("Subagent ended without a final text report.".into()),
                            text,
                        ),
                        _ => ("completed", None, text),
                    }
                }
                HarnessRunOutcome::Aborted { final_message, .. } => (
                    "cancelled",
                    Some("Subagent aborted.".into()),
                    assistant_text(&final_message),
                ),
                HarnessRunOutcome::Failed {
                    error,
                    final_message,
                    ..
                } => (
                    "failed",
                    Some(format!("Subagent failed: {}", error.message)),
                    final_message
                        .map(|m| assistant_text(&m))
                        .unwrap_or_default(),
                ),
                HarnessRunOutcome::Suspended { .. } => (
                    "failed",
                    Some("Subagent suspended unexpectedly.".into()),
                    String::new(),
                ),
            }
        }
        StopReasonKind::Finished(Err(error)) => {
            let _ = child.harness.drain_session().await;
            let _ = child.harness.close().await;
            (
                "failed",
                Some(format!("Subagent run failed: {error}")),
                String::new(),
            )
        }
        stop => {
            let _ = lane.abort().await;
            let _ = child.harness.close().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut run).await;
            let (status, notice) = match stop {
                StopReasonKind::Cancelled(reason) => ("cancelled", reason.to_owned()),
                StopReasonKind::TimedOut => (
                    "failed",
                    format!("Subagent timed out after {DEFAULT_TIMEOUT_SECONDS} seconds."),
                ),
                StopReasonKind::OutputLimit => ("failed", "Subagent output limit exceeded.".into()),
                StopReasonKind::Finished(_) => unreachable!(),
            };
            (status, Some(notice), String::new())
        }
    };
    let mut report = notice.unwrap_or_default();
    if text.trim().is_empty() && status != "completed" {
        let partial = observer.partial_text();
        if !partial.trim().is_empty() {
            report.push_str("\n\nLatest partial output:\n");
            report.push_str(&partial);
        }
    } else {
        if !report.is_empty() {
            report.push_str("\n\n");
        }
        report.push_str(&text);
    }
    finish_result(child_id, role, status, transcript_path, report)
}

fn finish_result(
    id: &str,
    role: &str,
    status: &str,
    transcript: Option<&Path>,
    mut report: String,
) -> Result<AgentToolResult, AgentError> {
    if report.len() > MAX_REPORT_BYTES {
        report.truncate(prefix_end(&report, MAX_REPORT_BYTES));
        report.push_str("\n\n[Report truncated.]");
    }
    let details = progress_details(id, role, status, transcript, Some(&report));
    if let Some(path) = transcript {
        report.push_str(&format!("\n\nSubagent transcript: {}", path.display()));
    } else {
        report.push_str("\n\nNo durable subagent transcript was saved.");
    }
    let result = AgentToolResult {
        content: vec![TextContentOrImage::text(report)],
        details,
        ..Default::default()
    };
    if status == "completed" {
        Ok(result)
    } else {
        Err(AgentError::StructuredTool(Box::new(result)))
    }
}

struct ChildObserver {
    child_id: String,
    role: String,
    transcript_path: Option<PathBuf>,
    on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    limit_exceeded: CancellationToken,
    changed: tokio::sync::Notify,
    state: Mutex<ObserverState>,
}

#[derive(Default)]
struct ObserverState {
    bytes: usize,
    turns: u32,
    activity: Option<String>,
    preview: String,
    last_report: String,
}

impl ChildObserver {
    fn new(
        child_id: String,
        role: String,
        transcript_path: Option<PathBuf>,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Self {
        Self {
            child_id,
            role,
            transcript_path,
            on_update,
            limit_exceeded: CancellationToken::new(),
            changed: tokio::sync::Notify::new(),
            state: Mutex::new(ObserverState::default()),
        }
    }

    fn observe(&self, event: AgentEvent) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            AgentEvent::TurnStart => {
                state.turns += 1;
                state.activity = None;
                state.preview.clear();
            }
            AgentEvent::TurnEnd {
                message: agent::AgentMessage::Assistant(message),
                ..
            } => {
                let mut text = assistant_text(&message);
                if !text.trim().is_empty() {
                    if text.len() > MAX_REPORT_BYTES {
                        text.truncate(prefix_end(&text, MAX_REPORT_BYTES));
                        text.push_str("\n\n[Partial output truncated.]");
                    }
                    state.last_report = text;
                }
                return;
            }
            AgentEvent::RetryScheduled { attempt, .. } => {
                state.preview.clear();
                state.activity = Some(format!("Retrying provider request ({attempt})"));
            }
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                state.activity = Some(format!("Running {tool_name}"));
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } if !delta.is_empty() => {
                state.bytes = state.bytes.saturating_add(delta.len());
                if state.bytes > MAX_OUTPUT_BYTES {
                    self.limit_exceeded.cancel();
                    return;
                }
                state.preview.push_str(tail(&delta, MAX_PREVIEW_BYTES));
                if state.preview.len() > MAX_PREVIEW_BYTES * 2 {
                    let start = tail_start(&state.preview, MAX_PREVIEW_BYTES);
                    state.preview.drain(..start);
                }
            }
            _ => return,
        }
        self.changed.notify_one();
    }

    fn partial_text(&self) -> String {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.last_report.is_empty() {
            state.last_report.clone()
        } else {
            tail(&state.preview, MAX_PREVIEW_BYTES).to_owned()
        }
    }

    async fn publish_updates(&self) {
        loop {
            self.changed.notified().await;
            let text = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let mut text = format!("{} (turn {})", self.role, state.turns);
                if let Some(activity) = &state.activity {
                    text.push_str(": ");
                    text.push_str(activity);
                }
                let preview = tail(&state.preview, MAX_PREVIEW_BYTES);
                if !preview.trim().is_empty() {
                    text.push('\n');
                    if preview.len() < state.preview.len() {
                        text.push('…');
                    }
                    text.push_str(preview);
                }
                text
            };
            (self.on_update)(ToolResultPartial {
                content: vec![TextContentOrImage::text(text)],
                details: progress_details(
                    &self.child_id,
                    &self.role,
                    "running",
                    self.transcript_path.as_deref(),
                    None,
                ),
                ..Default::default()
            });
            tokio::time::sleep(UPDATE_INTERVAL).await;
        }
    }
}

fn prefix_end(text: &str, max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn tail_start(text: &str, bytes: usize) -> usize {
    let mut start = text.len().saturating_sub(bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

fn tail(text: &str, bytes: usize) -> &str {
    &text[tail_start(text, bytes)..]
}

struct ChildEmitter(Arc<ChildObserver>);

impl AgentEmitter for ChildEmitter {
    fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        self.0.observe(event);
        Box::pin(async {})
    }

    fn try_emit(&self, event: AgentEvent) {
        self.0.observe(event);
    }
}

fn progress_details(
    id: &str,
    role: &str,
    status: &str,
    transcript: Option<&Path>,
    result_text: Option<&str>,
) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("id".into(), Value::String(id.to_owned()));
    entry.insert("label".into(), Value::String(role.to_owned()));
    entry.insert("status".into(), Value::String(status.to_owned()));
    if let Some(path) = transcript {
        entry.insert(
            "transcript".into(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
    if let Some(text) = result_text {
        entry.insert("resultText".into(), Value::String(text.to_owned()));
    }
    json!({ "progress": [Value::Object(entry)] })
}

fn sanitize_id(raw: &str, fallback: &str) -> String {
    let clean: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    if clean.is_empty() {
        fallback.to_owned()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai::providers::faux::{
        faux_assistant_message, FauxBlock, FauxProvider, FauxScript, FauxStep,
    };
    use ai::types::{Content, Message};
    use harness::agent_harness::AgentLane;
    use harness::types::AgentHarnessOptions;

    async fn test_parent(
        script: FauxScript,
        session_dir: Option<PathBuf>,
    ) -> (
        AgentHarness,
        Arc<FauxProvider>,
        Arc<OnceLock<WeakAgentHarness>>,
        Arc<tools::InMemoryExecutionEnv>,
    ) {
        let provider = FauxProvider::new(script);
        let env = Arc::new(tools::InMemoryExecutionEnv::new());
        let ctx = tools::ExecutionToolContext::new(env.clone(), Some(env.clone()));
        let harness_cell = Arc::new(OnceLock::new());
        let subagent =
            SubagentTool::create(Arc::clone(&harness_cell), std::env::temp_dir(), session_dir);
        let tools = vec![
            harness::types::HarnessTool::new(tools::create_read_tool(&ctx, None)),
            harness::types::HarnessTool::new(subagent),
        ];
        let session = Session::new(
            Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: "parent-session".into(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )),
            None,
        );
        let parent = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            tools,
            session,
            system_prompt: Some("You are a test harness.".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        let _ = harness_cell.set(parent.downgrade());
        (parent, provider, harness_cell, env)
    }

    #[test]
    fn prepare_arguments_normalizes_alias_fields() {
        let cell = Arc::new(OnceLock::new());
        let tool = SubagentTool::create(cell, PathBuf::from("/tmp"), None);
        let normalized = tool
            .prepare_arguments(json!({
                "prompt": "Inspect main.rs",
                "subagent_type": "Explorer",
                "max_turns": 4,
                "tools": ["read"]
            }))
            .unwrap();
        assert_eq!(
            normalized,
            json!({
                "task": "Inspect main.rs",
                "role": "Explorer",
                "maxTurns": 4,
                "tools": ["read"]
            })
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_tool_runs_child_and_writes_durable_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let script = FauxScript::new();
        script.set_responses(vec![
            FauxStep::tool_call(
                "subagent",
                json!({
                    "role": "Researcher",
                    "task": "Read hello.txt and summarize",
                    "tools": ["read"],
                    "maxTurns": 4
                }),
            ),
            FauxStep::tool_call("read", json!({"path": "hello.txt"})),
            FauxStep::text("Child summary: hello world"),
            FauxStep::factory(|ctx| {
                let Some(Message::ToolResult(result)) = ctx.messages.last() else {
                    panic!("parent did not receive the worker tool result");
                };
                assert!(!result.is_error);
                let [Content::Text(result)] = result.content.as_slice() else {
                    panic!("expected a text report");
                };
                assert!(result
                    .text
                    .starts_with("Child summary: hello world\n\nSubagent transcript: "));
                faux_assistant_message("Parent done.", StopReason::Stop)
            }),
        ]);
        let (parent, _provider, harness_cell, env) =
            test_parent(script, Some(dir.path().to_path_buf())).await;
        use tools::FileSystem;
        env.write_file(
            "hello.txt",
            tools::FileContent::Text("hello world".into()),
            None,
        )
        .await
        .unwrap();

        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        struct ChannelEmitter(tokio::sync::mpsc::UnboundedSender<AgentEvent>);
        impl AgentEmitter for ChannelEmitter {
            fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
                let _ = self.0.send(event);
                Box::pin(async {})
            }
            fn try_emit(&self, event: AgentEvent) {
                let _ = self.0.send(event);
            }
        }
        parent
            .set_agent_emitter(Some(Arc::new(ChannelEmitter(events_tx))))
            .await
            .unwrap();

        let outcome = parent
            .prompt_text("Delegate reading hello.txt", vec![])
            .await
            .unwrap();
        assert!(matches!(
            outcome.outcome,
            HarnessRunOutcome::Completed { .. }
        ));

        let mut saw_running_update = false;
        let mut saw_completed_end = false;
        let mut transcript_path: Option<PathBuf> = None;
        while let Ok(event) = events_rx.try_recv() {
            match event {
                AgentEvent::ToolExecutionUpdate {
                    tool_name,
                    partial_result,
                    ..
                } if tool_name == "subagent" => {
                    if partial_result.details["progress"][0]["status"] == "running" {
                        saw_running_update = true;
                        transcript_path = partial_result.details["progress"][0]["transcript"]
                            .as_str()
                            .map(PathBuf::from);
                    }
                }
                AgentEvent::ToolExecutionEnd {
                    tool_name,
                    result,
                    is_error,
                    ..
                } if tool_name == "subagent" => {
                    assert!(!is_error);
                    assert_eq!(result.details["progress"][0]["status"], "completed");
                    assert_eq!(result.details["progress"][0]["label"], "Researcher");
                    assert_eq!(
                        result.details["progress"][0]["resultText"],
                        "Child summary: hello world"
                    );
                    transcript_path = result.details["progress"][0]["transcript"]
                        .as_str()
                        .map(PathBuf::from);
                    saw_completed_end = true;
                }
                _ => {}
            }
        }
        assert!(saw_running_update, "expected live running update");
        assert!(saw_completed_end, "expected completed tool end");
        let transcript_path = transcript_path.expect("expected durable transcript path");
        assert!(
            transcript_path.is_file(),
            "transcript file must exist on disk"
        );
        let content = std::fs::read_to_string(&transcript_path).unwrap();
        assert!(content.contains("Child summary: hello world"));

        // Verify weak harness reference allows clean drop with no cycle.
        let weak = Arc::clone(&harness_cell);
        parent.close().await.unwrap();
        drop(parent);
        assert!(weak.get().unwrap().upgrade().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incomplete_worker_reports_reach_the_parent_as_errors() {
        for (step, max_turns, notice) in [
            (FauxStep::text(" "), 3, "without a final text report"),
            (
                FauxStep::tool_call("read", json!({"path":"two.txt"})),
                2,
                "stopped after using tools without a final report",
            ),
        ] {
            let script = FauxScript::new();
            script.set_responses(vec![
                FauxStep::tool_call(
                    NAME,
                    json!({"task":"Check the files", "maxTurns":max_turns}),
                ),
                FauxStep::message(faux_assistant_message(
                    vec![
                        FauxBlock::text("Useful lead from an earlier turn."),
                        FauxBlock::tool_call("read", json!({"path":"one.txt"})),
                    ],
                    StopReason::ToolUse,
                )),
                step,
                FauxStep::factory(move |ctx| {
                    let Some(Message::ToolResult(result)) = ctx.messages.last() else {
                        panic!("parent did not receive the worker tool result");
                    };
                    assert!(result.is_error);
                    let [Content::Text(result)] = result.content.as_slice() else {
                        panic!("expected a text report");
                    };
                    assert!(result.text.contains(notice), "{}", result.text);
                    assert!(result.text.contains("Useful lead from an earlier turn."));
                    faux_assistant_message("Parent understood.", StopReason::Stop)
                }),
            ]);
            let (parent, _, _, _) = test_parent(script, None).await;
            let result = parent
                .prompt_text("Delegate file check", vec![])
                .await
                .unwrap();
            assert!(
                matches!(result.outcome, HarnessRunOutcome::Completed { final_message, .. }
                if assistant_text(&final_message) == "Parent understood.")
            );
        }
    }

    #[tokio::test]
    async fn worker_text_is_available_in_a_bounded_live_snapshot() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let observer = ChildObserver::new(
            "worker".into(),
            "Explorer".into(),
            None,
            Arc::new(move |update| {
                tx.try_send(update).unwrap();
            }),
        );
        let message = Arc::new(faux_assistant_message("", StopReason::Stop));
        for delta in ["界".repeat(MAX_PREVIEW_BYTES), "live report".to_owned()] {
            observer.observe(AgentEvent::MessageUpdate {
                message: message.clone(),
                message_id: None,
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta,
                    partial: message.clone(),
                },
            });
        }
        let update = tokio::select! {
            _ = observer.publish_updates() => unreachable!(),
            update = rx.recv() => update.unwrap(),
        };
        let [TextContentOrImage::Text(text)] = update.content.as_slice() else {
            panic!("expected text preview");
        };
        assert!(text.text.ends_with("live report"));
        assert!(text.text.len() < MAX_PREVIEW_BYTES + 100);
        assert_eq!(update.details["progress"][0]["status"], "running");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subagent_cannot_nest_inside_child() {
        let cell = Arc::new(OnceLock::new());
        let tool = SubagentTool::create(cell, std::env::temp_dir(), None);
        let scope = Arc::new(agent::tool_execution_scope::ToolExecutionScope::default());
        let err = scope
            .run(tool.execute(
                "call-1",
                json!({"task": "Try nested"}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nested subagents are not allowed"));
    }
}
