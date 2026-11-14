//! Host-owned, tool-free, in-memory side conversations. Never writes the parent.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use rpi_agent::{AgentEmitter, AgentEvent};
use rpi_ai::types::{AssistantMessage, AssistantMessageEvent, UserMessage, UserRole};
use rpi_extensions::SideQuestionSink;
use rpi_harness::agent_harness::{AgentHarness, ChildHarness, HarnessRunOutcome};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::session::DefaultIdGenerator;
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::Session;
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT: usize = 256 * 1024;
const MAX_OWNERS: usize = 16;
const MAX_QUEUE: usize = 8;
const MAX_HISTORY: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Default)]
pub(crate) struct SideQuestions(Arc<Inner>);
struct Inner {
    registry: Mutex<Registry>,
    capacity: Arc<Semaphore>,
    revision: Arc<AtomicU64>,
}
impl Default for Inner {
    fn default() -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            capacity: Arc::new(Semaphore::new(8)),
            revision: Arc::new(AtomicU64::new(1)),
        }
    }
}
#[derive(Default)]
struct Registry {
    threads: HashMap<u64, Arc<Thread>>,
    revoked: HashSet<u64>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        for thread in self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .threads
            .values()
        {
            thread.retire();
        }
    }
}
struct Thread {
    owner: u64,
    id: String,
    session_id: String,
    reference: String,
    sink: SideQuestionSink,
    runtime: tokio::runtime::Handle,
    retired: CancellationToken,
    revision: Arc<AtomicU64>,
    state: Mutex<ThreadState>,
}
struct ThreadState {
    turns: Vec<Arc<Job>>,
    pending: VecDeque<Prepared>,
    active: Option<Arc<Job>>,
    worker: bool,
    visible: bool,
    focus_revision: u64,
    history_truncated: bool,
    epoch: u64,
}
struct Prepared {
    job: Arc<Job>,
    child: ChildHarness,
}
struct Job {
    id: String,
    question: String,
    cancel: CancellationToken,
    revision: Arc<AtomicU64>,
    state: Mutex<State>,
}
struct State {
    status: &'static str,
    text: String,
    error: Option<String>,
    response: Option<AssistantMessage>,
    deltas: usize,
}
impl Job {
    fn new(id: String, question: String, revision: Arc<AtomicU64>) -> Self {
        Self {
            id,
            question,
            cancel: CancellationToken::new(),
            revision,
            state: Mutex::new(State {
                status: "queued",
                text: String::new(),
                error: None,
                response: None,
                deltas: 0,
            }),
        }
    }
    fn snapshot(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        json!({"requestId":self.id,"question":self.question,"text":state.text,
            "status":state.status,"error":state.error})
    }
    fn finish(&self, status: &'static str, error: Option<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(state.status, "queued" | "running") {
            return;
        }
        state.status = status;
        state.error = error;
        self.cancel.cancel();
        self.revision.fetch_add(1, Ordering::SeqCst);
    }
    fn observe(&self, event: AgentEvent) {
        if let AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } = event
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.status != "running" {
                return;
            }
            if state.text.len().saturating_add(delta.len()) > MAX_OUTPUT || state.deltas >= 8192 {
                state.status = "failed";
                state.error = Some("side question output limit exceeded".into());
                self.cancel.cancel();
            } else {
                state.text.push_str(&delta);
                state.deltas += 1;
            }
            self.revision.fetch_add(1, Ordering::SeqCst);
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
impl Thread {
    fn changed(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::SeqCst) + 1
    }
    fn check(&self) -> Result<(), String> {
        if self.retired.is_cancelled() {
            return Err("side thread is no longer active".into());
        }
        // This check runs outside state locks: bridge invalidation may call retire().
        (self.sink)(json!({"op":"check"}))?;
        Ok(())
    }
    fn snapshot(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let turns: Vec<_> = state.turns.iter().map(|j| j.snapshot()).collect();
        let last = turns
            .last()
            .cloned()
            .unwrap_or_else(|| json!({"status":"idle","text":"","question":""}));
        json!({"owner":self.owner,"threadId":self.id,"sessionId":self.session_id,
            "revision":self.revision.load(Ordering::SeqCst),"focusRevision":state.focus_revision,
            "visible":state.visible,"turns":turns,"queued":state.pending.iter().map(|p| &p.job.question).collect::<Vec<_>>(),
            "status":last["status"],"requestId":last["requestId"],"question":last["question"],
            "text":last["text"],"error":last["error"],"historyTruncated":state.history_truncated})
    }
    fn cancel(&self) {
        let pending = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.epoch += 1;
            if let Some(job) = &state.active {
                job.finish("cancelled", None);
            }
            state.pending.drain(..).collect::<Vec<_>>()
        };
        // No provider calls while holding the thread or registry mutex.
        for request in pending {
            request.job.finish("cancelled", None);
            self.runtime.spawn(async move {
                let _ = request.child.harness.close().await;
            });
        }
        self.changed();
    }
    fn retire(&self) {
        self.retired.cancel();
        self.cancel();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.visible = false;
        state.turns.clear();
        self.changed();
    }
    fn trim(state: &mut ThreadState) {
        let mut bytes = 0usize;
        let mut count = 0;
        for job in &state.turns {
            let s = job.state.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(s.status, "done" | "failed" | "cancelled") {
                count += 1;
                bytes += job.question.len() + s.text.len();
            }
        }
        while count > 16 || bytes > MAX_HISTORY {
            let Some(index) = state.turns.iter().position(|job| {
                matches!(
                    job.state.lock().unwrap_or_else(|e| e.into_inner()).status,
                    "done" | "failed" | "cancelled"
                )
            }) else {
                break;
            };
            let job = state.turns.remove(index);
            bytes -= job.question.len()
                + job
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .text
                    .len();
            count -= 1;
            state.history_truncated = true;
        }
    }
}
impl SideQuestions {
    pub(crate) fn revision(&self) -> u64 {
        self.0.revision.load(Ordering::SeqCst)
    }
    pub(crate) fn views(&self) -> Vec<Value> {
        self.0
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .threads
            .values()
            .filter(|t| !t.retired.is_cancelled())
            .map(|t| t.snapshot())
            .collect()
    }
    pub(crate) fn cancel_owner(&self, owner: u64) {
        let thread = {
            let mut registry = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry.revoked.insert(owner);
            registry.threads.remove(&owner)
        };
        if let Some(thread) = thread {
            thread.retire();
        }
    }
    pub(crate) async fn ui_handle(
        &self,
        parent: &AgentHarness,
        owner: u64,
        thread_id: String,
        mut args: Value,
    ) -> Result<Value, String> {
        let thread = {
            let registry = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry
                .threads
                .get(&owner)
                .filter(|t| {
                    t.id == thread_id && t.session_id == parent.session().storage().metadata().id
                })
                .cloned()
                .ok_or("stale side thread UI")?
        };
        thread.check()?;
        // The identity is checked again after asynchronous preparation in handle().
        args["threadId"] = thread_id.into();
        self.handle(parent, owner, args, thread.sink.clone()).await
    }
    pub(crate) async fn handle(
        &self,
        parent: &AgentHarness,
        owner: u64,
        args: Value,
        sink: SideQuestionSink,
    ) -> Result<Value, String> {
        let op = args["op"]
            .as_str()
            .ok_or("missing side question operation")?;
        if op == "capabilities" {
            (sink)(json!({"op":"check"}))?;
            if self
                .0
                .registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .revoked
                .contains(&owner)
            {
                return Err("owning plugin unloaded".into());
            }
            return Ok(json!({"protocolVersion":2,"sideChat":true,"queuedFollowUps":true}));
        }
        if !matches!(
            op,
            "start" | "new" | "clear" | "status" | "cancel" | "hide" | "show"
        ) {
            return Err("unknown side question operation; upgrade rpi and rpi-btw together".into());
        }
        let question = if matches!(op, "start" | "new") {
            Some(
                args["question"]
                    .as_str()
                    .map(str::trim)
                    .filter(|q| !q.is_empty() && q.len() <= 20_000)
                    .ok_or("question must contain 1-20000 UTF-8 bytes")?
                    .to_owned(),
            )
        } else {
            None
        };
        (sink)(json!({"op":"check"}))?;
        let session_id = parent.session().storage().metadata().id.clone();
        let existing = {
            let mut registry = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
            if registry.revoked.contains(&owner) {
                return Err("owning plugin unloaded".into());
            }
            let existing = registry.threads.get(&owner).cloned();
            if let Some(expected) = args["threadId"].as_str() {
                if !existing
                    .as_ref()
                    .is_some_and(|t| t.id == expected && !t.retired.is_cancelled())
                {
                    return Err("stale side thread UI".into());
                }
            }
            if let Some(t) = &existing {
                if t.session_id != session_id
                    || t.retired.is_cancelled()
                    || matches!(op, "new" | "clear")
                {
                    let old = registry.threads.remove(&owner).unwrap();
                    // retire below, outside the registry gate.
                    Some((old, true))
                } else {
                    Some((t.clone(), false))
                }
            } else {
                None
            }
        };
        let existing = match existing {
            Some((old, true)) => {
                old.retire();
                None
            }
            Some((t, false)) => Some(t),
            None => None,
        };
        if op == "clear" {
            return Ok(json!({"status":"idle","text":"","queued":[]}));
        }
        if existing.is_none() && !matches!(op, "start" | "new" | "show") {
            return Ok(json!({"status":"idle","text":"","queued":[]}));
        }
        let thread = if let Some(thread) = existing {
            thread
        } else {
            let id = parent.session().id_generator().next();
            let anchor_job = Arc::new(Job::new(id.clone(), String::new(), self.0.revision.clone()));
            let session = memory_session(&id, &session_id);
            let (anchor, reference) = parent
                .create_side_question(session, Arc::new(Emitter(anchor_job)))
                .await
                .map_err(|e| e.to_string())?;
            let scope = anchor.scope.clone();
            let _ = anchor.harness.close().await;
            let candidate = Arc::new(Thread {
                owner,
                id,
                session_id: session_id.clone(),
                reference,
                sink: sink.clone(),
                runtime: tokio::runtime::Handle::current(),
                retired: CancellationToken::new(),
                revision: self.0.revision.clone(),
                state: Mutex::new(ThreadState {
                    turns: Vec::new(),
                    pending: VecDeque::new(),
                    active: None,
                    worker: false,
                    visible: true,
                    focus_revision: self.revision() + 1,
                    history_truncated: false,
                    epoch: 0,
                }),
            });
            let selected = {
                let mut registry = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
                if registry.revoked.contains(&owner)
                    || scope.is_cancelled()
                    || parent.session().storage().metadata().id != session_id
                {
                    return Err("owning plugin or parent session changed".into());
                }
                registry.threads.retain(|_, t| !t.retired.is_cancelled());
                if let Some(current) = registry.threads.get(&owner) {
                    current.clone()
                } else {
                    if registry.threads.len() >= MAX_OWNERS {
                        return Err("side question owner limit reached".into());
                    }
                    registry.threads.insert(owner, candidate.clone());
                    candidate.clone()
                }
            };
            if Arc::ptr_eq(&selected, &candidate) {
                let weak = Arc::downgrade(&candidate);
                let registry = Arc::downgrade(&self.0);
                let retired = candidate.retired.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = retired.cancelled() => {},
                        _ = scope.cancelled() => {
                            if let Some(t) = weak.upgrade() {
                                if let Some(inner) = registry.upgrade() {
                                    let mut state = inner.registry.lock().unwrap_or_else(|e| e.into_inner());
                                    if state.threads.get(&owner).is_some_and(|current| Arc::ptr_eq(current, &t)) {
                                        state.threads.remove(&owner);
                                    }
                                }
                                t.retire();
                            }
                        },
                    }
                });
                candidate.changed();
            }
            selected
        };
        thread.check()?;
        match op {
            "cancel" => thread.cancel(),
            "hide" | "show" => {
                let mut state = thread.state.lock().unwrap_or_else(|e| e.into_inner());
                state.visible = op == "show";
                let revision = thread.changed();
                if state.visible {
                    state.focus_revision = revision;
                }
            }
            "start" | "new" => {
                let job = Arc::new(Job::new(
                    parent.session().id_generator().next(),
                    question.unwrap(),
                    self.0.revision.clone(),
                ));
                // Prepare immediately to pin this submission's private model/provider binding,
                // including queued requests. Only the serial worker runs its lane.
                let epoch = thread.state.lock().unwrap_or_else(|e| e.into_inner()).epoch;
                let (child, _) = parent
                    .create_side_question(
                        memory_session(&job.id, &session_id),
                        Arc::new(Emitter(job.clone())),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let mut prepared = Some(Prepared {
                    job: job.clone(),
                    child,
                });
                let admission = (|| {
                    thread.check()?;
                    if prepared.as_ref().unwrap().child.scope.is_cancelled()
                        || parent.session().storage().metadata().id != session_id
                    {
                        return Err("parent session changed".to_owned());
                    }
                    let mut state = thread.state.lock().unwrap_or_else(|e| e.into_inner());
                    if thread.retired.is_cancelled() || state.epoch != epoch {
                        return Err("side question cancelled during submission".into());
                    }
                    if state.pending.len() >= MAX_QUEUE {
                        return Err("side question queue is full (8 questions)".into());
                    }
                    let queued = state.worker;
                    let permit = if queued {
                        None
                    } else {
                        Some(
                            self.0
                                .capacity
                                .clone()
                                .try_acquire_owned()
                                .map_err(|_| "side question capacity/cleanup limit reached")?,
                        )
                    };
                    state.worker = true;
                    state.visible = true;
                    state.focus_revision = thread.changed();
                    state.pending.push_back(prepared.take().unwrap());
                    Ok((queued, permit))
                })();
                let (queued, permit) = match admission {
                    Ok(v) => v,
                    Err(error) => {
                        let _ = prepared.unwrap().child.harness.close().await;
                        return Err(error);
                    }
                };
                if let Some(permit) = permit {
                    let t = thread.clone();
                    tokio::spawn(async move {
                        worker(t, permit).await;
                    });
                }
                return Ok(
                    json!({"threadId":thread.id,"requestId":job.id,"status":if queued {"queued"} else {"running"},"queued":thread.snapshot()["queued"]}),
                );
            }
            _ => {}
        }
        Ok(thread.snapshot())
    }
}
fn memory_session(id: &str, parent: &str) -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: id.into(),
                created_at: 0,
                parent_session_id: Some(parent.into()),
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    )
}
async fn worker(thread: Arc<Thread>, _permit: OwnedSemaphorePermit) {
    loop {
        let next = {
            let mut state = thread.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(request) = state.pending.pop_front() {
                // Failed/cancelled turns stay visible (bounded by trim), but only
                // successful responses are replayed into subsequent requests.
                request
                    .job
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .status = "running";
                state.active = Some(request.job.clone());
                let history = state
                    .turns
                    .iter()
                    .filter_map(|j| {
                        j.state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .response
                            .clone()
                            .map(|r| (j.question.clone(), r))
                    })
                    .collect::<Vec<_>>();
                state.turns.push(request.job.clone());
                thread.changed();
                Some((request, history))
            } else {
                state.worker = false;
                state.active = None;
                thread.changed();
                None
            }
        };
        let Some((request, history)) = next else {
            break;
        };
        let execution_scope = request.child.execution_scope.clone();
        let cleanup = request.child.harness.clone();
        let job = request.job.clone();
        let prompt = format!("Conversation excerpt (reference only):\n<conversation-reference>\n{}</conversation-reference>\n\nSide question:\n{}", thread.reference, job.question);
        let outcome = std::panic::AssertUnwindSafe(async {
            for (question, response) in history {
                request
                    .child
                    .harness
                    .session()
                    .view("main")
                    .append_message(
                        UserMessage {
                            role: UserRole,
                            content: question.into(),
                            timestamp: 0,
                        }
                        .into(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                request
                    .child
                    .harness
                    .session()
                    .view("main")
                    .append_message(response.into())
                    .await
                    .map_err(|e| e.to_string())?;
            }
            if thread.check().is_err() {
                job.finish("cancelled", None);
            }
            run(request.child, job.clone(), prompt, TIMEOUT).await;
            Ok::<(), String>(())
        })
        .catch_unwind()
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => job.finish("failed", Some(error)),
            Err(_) => job.finish("failed", Some("side question runner panicked".into())),
        }
        let _ = cleanup.close().await;
        execution_scope.wait_idle().await;
        {
            let mut state = thread.state.lock().unwrap_or_else(|e| e.into_inner());
            state.active = None;
            Thread::trim(&mut state);
            thread.changed();
        }
    }
}
async fn run(child: ChildHarness, job: Arc<Job>, prompt: String, timeout: Duration) {
    let lane = child.harness.lane("main");
    let run = child
        .execution_scope
        .run(lane.prompt_text(&prompt, Vec::new()));
    tokio::pin!(run);
    let result = tokio::select! {
        biased;
        _ = job.cancel.cancelled() => { job.finish("cancelled", None); None },
        _ = child.scope.cancelled() => { job.finish("cancelled", Some("parent session closed or changed".into())); None },
        _ = tokio::time::sleep(timeout) => { job.finish("failed", Some("side question timed out".into())); None },
        result = &mut run => Some(result),
    };
    if let Some(result) = result {
        match result {
            Ok(result) => {
                let mut state = job.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.status == "running" {
                    match result.outcome {
                        HarnessRunOutcome::Completed { final_message, .. } => {
                            let text = crate::extensions_actions::assistant_text(&final_message);
                            if !matches!(
                                final_message.stop_reason,
                                rpi_ai::types::StopReason::Stop | rpi_ai::types::StopReason::Length
                            ) || final_message
                                .content
                                .iter()
                                .any(|b| matches!(b, rpi_ai::types::Content::ToolCall(_)))
                                || text.is_empty()
                            {
                                state.status = "failed";
                                state.error =
                                    Some("side question did not produce a final answer".into());
                            } else if text.len() > MAX_OUTPUT {
                                state.status = "failed";
                                state.error = Some("side question output limit exceeded".into());
                            } else {
                                state.text = text;
                                // Keep only text: no thinking, images or tool protocols in follow-ups.
                                let mut response = final_message;
                                response
                                    .content
                                    .retain(|b| matches!(b, rpi_ai::types::Content::Text(_)));
                                state.response = Some(response);
                                state.status = "done";
                            }
                        }
                        _ => {
                            state.status = "failed";
                            state.error =
                                Some("side question did not produce a final answer".into());
                        }
                    }
                    job.revision.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(error) => job.finish("failed", Some(format!("side question failed: {error}"))),
        }
    } else {
        let _ = lane.abort().await;
        let _ = child.harness.close().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut run).await;
    }
    let _ = child.harness.close().await;
}

#[cfg(test)]
mod tests;
