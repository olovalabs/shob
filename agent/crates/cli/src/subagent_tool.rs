//! Built-in `subagent` tool backed by `AgentHarness::create_child`.
//!
//! Runs bounded, isolated child sessions using the parent harness's active
//! model, provider/auth bindings, thinking level, cwd, and permission hooks.
//! Child sessions cannot recursively spawn subagents or expand capabilities.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use agent::{AgentEmitter, AgentError, AgentEvent, AgentTool};
use ai::types::{AssistantMessageEvent, Schema, Tool};
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
const MAX_EVENTS: usize = 8192;
const UPDATE_INTERVAL: Duration = Duration::from_millis(200);

pub(crate) fn prompt_metadata() -> extensions::ToolPromptMetadata {
    extensions::ToolPromptMetadata {
        prompt_snippet: Some(
            "Spawn a focused subagent to investigate, analyze, or execute an isolated subtask"
                .into(),
        ),
        prompt_guidelines: vec![
            "Use subagent when a task benefits from isolated context or parallel exploration (multiple independent subagent calls run concurrently)."
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
            .await
            .map_err(|error| AgentError::Tool(error.to_string()))?;

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
                Err(AgentError::StructuredTool(Box::new(AgentToolResult {
                    content: vec![TextContentOrImage::text("subagent runner panicked")],
                    details: progress_details(
                        &child_id,
                        &role,
                        "failed",
                        transcript_path.as_deref(),
                        Some("subagent runner panicked"),
                    ),
                    ..Default::default()
                })))
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
        result = &mut run => StopReasonKind::Finished(result),
    };

    let outcome = match stop {
        StopReasonKind::Finished(Ok(result)) => {
            let _ = child.harness.drain_session().await;
            let _ = child.harness.close().await;
            match result.outcome {
                HarnessRunOutcome::Completed { final_message, .. } => {
                    let text = assistant_text(&final_message);
                    let display = if text.trim().is_empty() {
                        "(subagent completed with no text output)".to_owned()
                    } else {
                        text
                    };
                    Ok(AgentToolResult {
                        content: vec![TextContentOrImage::text(display.clone())],
                        details: progress_details(
                            child_id,
                            role,
                            "completed",
                            transcript_path,
                            Some(&display),
                        ),
                        ..Default::default()
                    })
                }
                HarnessRunOutcome::Aborted { final_message, .. } => {
                    let partial = assistant_text(&final_message);
                    let msg = if partial.trim().is_empty() {
                        "subagent aborted".to_owned()
                    } else {
                        format!("subagent aborted\n\n{partial}")
                    };
                    Err(AgentError::StructuredTool(Box::new(AgentToolResult {
                        content: vec![TextContentOrImage::text(msg.clone())],
                        details: progress_details(
                            child_id,
                            role,
                            "cancelled",
                            transcript_path,
                            Some(&msg),
                        ),
                        ..Default::default()
                    })))
                }
                HarnessRunOutcome::Failed {
                    error,
                    final_message,
                    ..
                } => {
                    let partial = final_message
                        .map(|m| assistant_text(&m))
                        .unwrap_or_default();
                    let msg = if partial.trim().is_empty() {
                        format!("subagent failed: {error:?}")
                    } else {
                        format!("subagent failed: {error:?}\n\n{partial}")
                    };
                    Err(AgentError::StructuredTool(Box::new(AgentToolResult {
                        content: vec![TextContentOrImage::text(msg.clone())],
                        details: progress_details(
                            child_id,
                            role,
                            "failed",
                            transcript_path,
                            Some(&msg),
                        ),
                        ..Default::default()
                    })))
                }
                HarnessRunOutcome::Suspended { .. } => {
                    let msg = "subagent suspended unexpectedly".to_owned();
                    Err(AgentError::StructuredTool(Box::new(AgentToolResult {
                        content: vec![TextContentOrImage::text(msg.clone())],
                        details: progress_details(
                            child_id,
                            role,
                            "failed",
                            transcript_path,
                            Some(&msg),
                        ),
                        ..Default::default()
                    })))
                }
            }
        }
        StopReasonKind::Finished(Err(error)) => {
            let _ = child.harness.drain_session().await;
            let _ = child.harness.close().await;
            let msg = format!("subagent run failed: {error}");
            Err(AgentError::StructuredTool(Box::new(AgentToolResult {
                content: vec![TextContentOrImage::text(msg.clone())],
                details: progress_details(child_id, role, "failed", transcript_path, Some(&msg)),
                ..Default::default()
            })))
        }
        StopReasonKind::Cancelled(reason) => {
            let _ = lane.abort().await;
            let _ = child.harness.close().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut run).await;
            Err(AgentError::StructuredTool(Box::new(AgentToolResult {
                content: vec![TextContentOrImage::text(reason)],
                details: progress_details(
                    child_id,
                    role,
                    "cancelled",
                    transcript_path,
                    Some(reason),
                ),
                ..Default::default()
            })))
        }
        StopReasonKind::TimedOut => {
            let _ = lane.abort().await;
            let _ = child.harness.close().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut run).await;
            let msg = format!("subagent timed out after {DEFAULT_TIMEOUT_SECONDS} seconds");
            Err(AgentError::StructuredTool(Box::new(AgentToolResult {
                content: vec![TextContentOrImage::text(msg.clone())],
                details: progress_details(child_id, role, "failed", transcript_path, Some(&msg)),
                ..Default::default()
            })))
        }
        StopReasonKind::OutputLimit => {
            let _ = lane.abort().await;
            let _ = child.harness.close().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut run).await;
            let msg = "subagent output limit exceeded".to_owned();
            Err(AgentError::StructuredTool(Box::new(AgentToolResult {
                content: vec![TextContentOrImage::text(msg.clone())],
                details: progress_details(child_id, role, "failed", transcript_path, Some(&msg)),
                ..Default::default()
            })))
        }
    };
    observer.finished.store(true, Ordering::Release);
    outcome
}

struct ChildObserver {
    child_id: String,
    role: String,
    transcript_path: Option<PathBuf>,
    on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    limit_exceeded: CancellationToken,
    finished: AtomicBool,
    state: Mutex<ObserverState>,
}

struct ObserverState {
    bytes: usize,
    events: usize,
    turns: u32,
    last_emit: Instant,
    activity: Option<String>,
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
            finished: AtomicBool::new(false),
            state: Mutex::new(ObserverState {
                bytes: 0,
                events: 0,
                turns: 0,
                last_emit: Instant::now() - UPDATE_INTERVAL,
                activity: None,
            }),
        }
    }

    fn observe(&self, event: AgentEvent) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        let mut emit_text: Option<String> = None;
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let mut changed = false;
            match event {
                AgentEvent::TurnEnd { .. } => {
                    state.turns += 1;
                    changed = true;
                }
                AgentEvent::ToolExecutionStart { tool_name, .. } => {
                    state.activity = Some(format!("Running {tool_name}"));
                    changed = true;
                }
                AgentEvent::MessageUpdate {
                    assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                    ..
                } => {
                    if state.bytes.saturating_add(delta.len()) > MAX_OUTPUT_BYTES
                        || state.events >= MAX_EVENTS
                    {
                        self.limit_exceeded.cancel();
                        return;
                    }
                    state.bytes += delta.len();
                    state.events += 1;
                }
                _ => {}
            }
            if changed && state.last_emit.elapsed() >= UPDATE_INTERVAL {
                state.last_emit = Instant::now();
                let summary = match &state.activity {
                    Some(activity) if state.turns > 0 => {
                        format!("{} (turn {}): {activity}…", self.role, state.turns + 1)
                    }
                    Some(activity) => format!("{}: {activity}…", self.role),
                    None => format!("{} (turn {})…", self.role, state.turns + 1),
                };
                emit_text = Some(summary);
            }
        }
        if let Some(text) = emit_text {
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
        }
    }
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
    use ai::providers::faux::{FauxProvider, FauxScript, FauxStep};
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
            FauxStep::text("Parent done."),
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
