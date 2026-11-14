//! Bounded, plugin-owned child runs. Execution bindings stay inside the host.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use rpi_agent::{AgentEmitter, AgentEvent};
use rpi_ai::types::AssistantMessageEvent;
use rpi_harness::agent_harness::{AgentHarness, ChildHarness};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::session::DefaultIdGenerator;
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::Session;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const MAX_JOBS: usize = 16;
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_EVENTS: usize = 8192;

#[derive(Default)]
pub(crate) struct ChildAgents {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    revoked: Mutex<HashSet<u64>>,
    active: Arc<AtomicUsize>,
}

struct Permit(Arc<AtomicUsize>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Job {
    owner: u64,
    cancel: CancellationToken,
    invocation_signal: Option<CancellationToken>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    deltas: VecDeque<String>,
    bytes: usize,
    events: usize,
    turns: u32,
    result: Option<Value>,
    error: Option<String>,
}

impl Job {
    fn fail(&self, error: impl Into<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.result.is_none() && state.error.is_none() {
            state.error = Some(error.into());
        }
        self.cancel.cancel();
    }

    fn observe(&self, event: AgentEvent) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.result.is_some() || state.error.is_some() {
            return;
        }
        match event {
            AgentEvent::TurnEnd { .. } => state.turns += 1,
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => {
                if state.bytes.saturating_add(delta.len()) > MAX_OUTPUT
                    || state.events >= MAX_EVENTS
                {
                    state.error = Some("child output limit exceeded".into());
                    self.cancel.cancel();
                } else {
                    state.bytes += delta.len();
                    state.events += 1;
                    state.deltas.push_back(delta);
                }
            }
            _ => {}
        }
    }
}

struct Emitter(Arc<Job>);
impl AgentEmitter for Emitter {
    fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        self.0.observe(event);
        Box::pin(async {})
    }
    fn try_emit(&self, event: AgentEvent) {
        self.0.observe(event);
    }
}

impl Drop for ChildAgents {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

impl ChildAgents {
    pub(crate) fn cancel_owner(&self, owner: u64) {
        self.revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(owner);
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, job| {
                if job.owner == owner {
                    job.fail("owning plugin unloaded");
                    false
                } else {
                    true
                }
            });
    }

    pub(crate) fn cancel_all(&self) {
        for job in self.jobs.lock().unwrap_or_else(|e| e.into_inner()).values() {
            job.fail("parent session closed or changed");
        }
    }

    pub(crate) async fn handle(
        &self,
        parent: &AgentHarness,
        owner: u64,
        args: Value,
    ) -> Result<Value, String> {
        let op = args
            .get("op")
            .and_then(Value::as_str)
            .ok_or("missing child op")?;
        if op == "start" {
            return self.start(parent, owner, &args).await;
        }
        self.handle_existing(owner, args)
    }

    pub(crate) fn handle_existing(&self, owner: u64, args: Value) -> Result<Value, String> {
        let op = args
            .get("op")
            .and_then(Value::as_str)
            .ok_or("missing child op")?;
        let id = args
            .get("childId")
            .and_then(Value::as_str)
            .ok_or("missing childId")?;
        let job = {
            let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.get(id)
                .filter(|job| job.owner == owner)
                .cloned()
                .ok_or("unknown childId for this plugin")?
        };
        match op {
            "poll" => {
                let mut state = job.state.lock().unwrap_or_else(|e| e.into_inner());
                let status = if state.error.is_some() {
                    "failed"
                } else if state.result.is_some() {
                    "done"
                } else {
                    "running"
                };
                // Bound each ABI response as well as the lifetime output budget.
                let mut deltas = Vec::new();
                let mut bytes = 0;
                while let Some(delta) = state.deltas.front() {
                    if !deltas.is_empty() && bytes + delta.len() > 64 * 1024 {
                        break;
                    }
                    bytes += delta.len();
                    deltas.push(state.deltas.pop_front().expect("front exists"));
                }
                // Terminal result is delivered only after queued deltas drain.
                let drained = state.deltas.is_empty();
                Ok(
                    json!({"childId":id, "status":if drained {status} else {"running"}, "deltas":deltas,
                    "turns":state.turns, "result":if drained {state.result.clone()} else {None},
                    "error":if drained {state.error.clone()} else {None}}),
                )
            }
            "cancel" | "dispose" => {
                job.fail("child cancelled");
                if op == "dispose" {
                    self.jobs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(id);
                }
                Ok(
                    json!({"childId":id,"status":if op == "dispose" {"disposed"} else {"cancelled"}}),
                )
            }
            _ => Err(format!("unknown child op `{op}`")),
        }
    }

    async fn start(
        &self,
        parent: &AgentHarness,
        owner: u64,
        args: &Value,
    ) -> Result<Value, String> {
        let invocation_signal = rpi_agent::tool_execution_scope::invocation_signal();
        if invocation_signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err("parent tool invocation cancelled".into());
        }
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .ok_or("missing child task")?;
        let role = args
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("worker")
            .trim();
        if task.trim().is_empty() || task.len() > 20_000 || role.is_empty() || role.len() > 200 {
            return Err("child task must contain 1-20000 bytes and role 1-200 bytes".into());
        }
        let turns = bounded_number(args, "maxTurns", 8, 1, 32)? as u32;
        let timeout = bounded_number(args, "timeoutSeconds", 300, 10, 1800)?;
        let tools = args
            .get("tools")
            .map(|value| {
                value
                    .as_array()
                    .ok_or("child tools must be a string array")
                    .and_then(|values| {
                        values
                            .iter()
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .ok_or("child tools must be a string array")
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
            })
            .transpose()?;
        let id = parent.session().id_generator().next();
        let job = Arc::new(Job {
            owner,
            cancel: CancellationToken::new(),
            invocation_signal,
            state: Mutex::new(State::default()),
        });
        // Reserve before awaiting construction so concurrent starts cannot exceed the cap.
        let permit;
        {
            let revoked = self.revoked.lock().unwrap_or_else(|e| e.into_inner());
            if revoked.contains(&owner) {
                return Err("owning plugin unloaded".into());
            }
            let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            if jobs.len() >= MAX_JOBS {
                return Err("child capacity reached; dispose finished children".into());
            }
            let mut active = self.active.load(Ordering::Acquire);
            loop {
                if active >= MAX_JOBS {
                    return Err("child cleanup still in progress; retry later".into());
                }
                match self.active.compare_exchange_weak(
                    active,
                    active + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(current) => active = current,
                }
            }
            permit = Permit(self.active.clone());
            jobs.insert(id.clone(), job.clone());
        }
        let session = Session::new(
            Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: id.clone(),
                    created_at: 0,
                    parent_session_id: Some(parent.session().storage().metadata().id.clone()),
                },
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )),
            None,
        );
        let child = match parent
            .create_child(session, Arc::new(Emitter(job.clone())), turns, tools)
            .await
        {
            Ok(child) => child,
            Err(error) => {
                self.jobs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                return Err(error.to_string());
            }
        };
        let prompt = format!("You are a delegated {role} subagent. Complete the task in at most {turns} turns. Do not delegate, spawn, or ask another agent to do work. Work only in the current project and return a concise final report.\n\nTask:\n{task}");
        let worker = job.clone();
        let execution_scope = child.execution_scope.clone();
        let cleanup_harness = child.harness.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if std::panic::AssertUnwindSafe(run_child(
                child,
                worker.clone(),
                prompt,
                turns,
                timeout,
            ))
            .catch_unwind()
            .await
            .is_err()
            {
                worker.fail("child runner panicked");
                let _ = cleanup_harness.close().await;
            }
            // Also retain capacity when run_child unwinds after starting a
            // background driver; catch_unwind is not a cleanup completion.
            execution_scope.wait_idle().await;
        });
        Ok(json!({"childId":id,"sessionId":id,"status":"running"}))
    }
}

fn bounded_number(
    args: &Value,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let value = match args.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("{key} must be an integer"))?,
    };
    if !(min..=max).contains(&value) {
        return Err(format!("{key} must be between {min} and {max}"));
    }
    Ok(value)
}

async fn run_child(child: ChildHarness, job: Arc<Job>, prompt: String, turns: u32, timeout: u64) {
    let lane = child.harness.lane("main");
    let run = lane.prompt_text(&prompt, Vec::new());
    tokio::pin!(run);
    let parent_abort = async {
        match &child.parent_signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    let invocation_abort = async {
        match &job.invocation_signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    let result = tokio::select! {
        biased;
        _ = job.cancel.cancelled() => None,
        _ = invocation_abort => {job.fail("parent tool invocation cancelled"); None},
        _ = child.scope.cancelled() => {job.fail("parent run ended"); None},
        _ = parent_abort => {job.fail("parent run cancelled"); None},
        _ = tokio::time::sleep(Duration::from_secs(timeout)) => {job.fail(format!("child timed out after {timeout} seconds")); None},
        result = &mut run => Some(result),
    };
    if let Some(result) = result {
        match result {
            Ok(result) => {
                let mut outcome = crate::extensions_actions::run_outcome_json(result.outcome);
                let mut state = job.state.lock().unwrap_or_else(|e| e.into_inner());
                let bytes = outcome
                    .get("text")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
                if state.bytes.saturating_add(bytes) > MAX_OUTPUT {
                    state
                        .error
                        .get_or_insert_with(|| "child output limit exceeded".into());
                } else if state.error.is_none() {
                    outcome["turns"] = json!(state.turns);
                    outcome["turnLimited"] = json!(state.turns >= turns);
                    state.result = Some(outcome);
                }
            }
            Err(error) => job.fail(format!("child run failed: {error}")),
        }
    } else {
        // Abort even if cancellation raced ahead of the run's first poll. A closed
        // harness rejects admission; an admitted run gets its cancellation signal.
        let _ = lane.abort().await;
        let _ = child.harness.close().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut run).await;
    }
    let _ = child.harness.close().await;
}

#[cfg(test)]
mod tests;
