//! Host-owned, invocation-local nested execution. No model turns or transcript events.
use crate::tool_program_observation::{
    self, ToolProgramChild, ToolProgramObservation, ToolProgramObserver,
};
use crate::{
    AgentContext, AgentEmitter, AgentEvent, AgentLoopConfig, TextContentOrImage, ToolExecutionMode,
};
use futures::{future::BoxFuture, FutureExt};
use rpi_ai::types::{AssistantMessage, ToolCall, ToolCallType};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::sync::{Notify, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

#[path = "tool_program_accounting.rs"]
mod accounting;
#[path = "tool_program_results.rs"]
mod results;

/// Native content composition. References select only content, never child metadata.
#[derive(Clone, Debug)]
pub enum ToolProgramContentPart {
    /// Adapter-validated owned blocks; plugin frames remain limited to 1 MiB.
    Inline(Vec<TextContentOrImage>),
    /// Invocation-owned native content. None selects all; an empty selection
    /// selects none. Ordered indices may repeat, but must remain in bounds.
    Reference {
        id: String,
        indices: Option<Vec<usize>>,
    },
}

const MAX_BYTES: usize = 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(1);
tokio::task_local! { static CURRENT: Arc<ToolProgram>; static PARENT: NestedParent; }

/// Identity only the native execution driver can install; JSON is never trusted.
#[derive(Clone)]
pub struct NestedParent {
    pub root_id: String,
    pub root_name: String,
    pub parent_id: String,
}
pub fn nested_parent() -> Option<NestedParent> {
    PARENT.try_with(Clone::clone).ok()
}
pub fn current() -> Option<Arc<ToolProgram>> {
    CURRENT.try_with(Clone::clone).ok()
}
pub(crate) async fn scope<F: Future>(capability: Arc<ToolProgram>, future: F) -> F::Output {
    CURRENT.scope(capability, future).await
}
pub(crate) struct InvocationLifetime(pub CancellationToken);
impl Drop for InvocationLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct Request {
    signal: CancellationToken,
    result: Option<Value>,
}
#[derive(Default)]
struct State {
    requests: HashMap<String, Request>,
    accounting: accounting::Accounting,
    results: results::Store,
}
/// Captured from the effective executing loop, never from a root registry.
pub struct ToolProgram {
    context: Arc<AgentContext>,
    assistant: Arc<AssistantMessage>,
    config: AgentLoopConfig,
    ancestors: Vec<String>,
    identity: NestedParent,
    child_scope: Option<Arc<crate::tool_execution_scope::ToolExecutionScope>>,
    observer: Option<Arc<dyn ToolProgramObserver>>,
    starts: Arc<AtomicUsize>,
    state: Mutex<State>,
    serial: RwLock<()>,
    active: Arc<Semaphore>,
    tree_active: Arc<Semaphore>,
    result_bytes: Arc<Semaphore>,
    ordered: bool,
    terminated: AtomicBool,
    progress: Arc<Progress>,
    pub(crate) signal: CancellationToken,
}
impl ToolProgram {
    pub(crate) fn capture(
        context: &Arc<AgentContext>,
        assistant: &Arc<AssistantMessage>,
        call: &ToolCall,
        config: &AgentLoopConfig,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(crate::ToolResultPartial) + Send + Sync>,
        emit: &Arc<dyn AgentEmitter>,
    ) -> Arc<Self> {
        // Closing this composition must not cancel its containing native tool:
        // wrappers may still need that tool's signal for result postprocessing.
        // Caller cancellation still propagates down into the program and children.
        let signal = signal.child_token();
        let previous = current();
        let mut ancestors = previous
            .as_ref()
            .map(|p| p.ancestors.clone())
            .unwrap_or_default();
        ancestors.push(call.name.clone());
        let identity = previous
            .as_ref()
            .map(|p| NestedParent {
                root_id: p.identity.root_id.clone(),
                root_name: p.identity.root_name.clone(),
                parent_id: call.id.clone(),
            })
            .unwrap_or_else(|| NestedParent {
                root_id: call.id.clone(),
                root_name: call.name.clone(),
                parent_id: call.id.clone(),
            });
        Arc::new(Self {
            context: context.clone(),
            assistant: assistant.clone(),
            config: config.clone(),
            ancestors,
            identity,
            child_scope: crate::tool_execution_scope::current_scope(),
            // Nested execution inherits only the actual containing capture.
            observer: match &previous {
                Some(parent) => parent.observer.clone(),
                None => emit.tool_program_observer(),
            },
            starts: previous
                .as_ref()
                .map(|p| p.starts.clone())
                .unwrap_or_default(),
            tree_active: previous
                .as_ref()
                .map(|p| p.tree_active.clone())
                .unwrap_or_else(|| Arc::new(Semaphore::new(8))),
            result_bytes: previous
                .as_ref()
                .map(|p| p.result_bytes.clone())
                .unwrap_or_else(|| Arc::new(Semaphore::new(results::TREE_BYTES))),
            ordered: config.tool_execution == ToolExecutionMode::Sequential
                || context.tools.iter().any(|t| {
                    t.schema().name == call.name
                        && t.execution_mode() == ToolExecutionMode::Sequential
                }),
            state: Mutex::default(),
            serial: RwLock::new(()),
            active: Arc::new(Semaphore::new(8)),
            terminated: AtomicBool::new(false),
            progress: previous
                .as_ref()
                .map(|p| p.progress.clone())
                .unwrap_or_else(|| {
                    Arc::new(Progress {
                        callback: on_update,
                        signal: signal.clone(),
                        budget: Mutex::default(),
                    })
                }),
            signal,
        })
    }
    /// Effective names, including this tool, for the existing names-only action.
    pub fn active_tool_names(&self) -> Vec<String> {
        self.context
            .tools
            .iter()
            .map(|t| t.schema().name.clone())
            .collect()
    }
    /// Cancellation stops admission. Cleanup/poll remain available to the binding.
    pub fn cancel_all(&self) {
        self.signal.cancel();
    }
    /// Observe driver-local cancellation, without retaining a finished invocation.
    pub fn bind_signal(self: &Arc<Self>, signal: CancellationToken) {
        let weak = Arc::downgrade(self);
        let finished = self.signal.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = signal.cancelled() => {
                    if let Some(program) = weak.upgrade() { program.cancel_all(); }
                }
                _ = finished.cancelled() => {}
            }
        });
    }
    pub(crate) async fn finish(&self) {
        self.cancel_all();
        // An outer Sequential call cannot finish ahead of its descendants.
        let _idle = self.active.acquire_many(8).await;
        self.state.lock().unwrap().results.clear();
    }
    pub(crate) fn merge_accounting(
        &self,
        result: &mut crate::AgentToolResult,
    ) -> Result<(), String> {
        self.state.lock().unwrap().accounting.merge(result)
    }
    pub fn handle(self: &Arc<Self>, args: Value) -> BoxFuture<'static, Result<Value, String>> {
        let this = self.clone();
        Box::pin(async move { this.handle_inner(args).await })
    }
    async fn handle_inner(self: &Arc<Self>, args: Value) -> Result<Value, String> {
        bounded(&args)?;
        let op = args["op"].as_str().ok_or("op is required")?;
        if matches!(op, "list" | "start")
            && (self.signal.is_cancelled() || self.terminated.load(Ordering::SeqCst))
        {
            return Err("parent invocation is no longer active".into());
        }
        match op {
            "list" => {
                let offset = integer(&args, "offset", 0, 0, 4096)? as usize;
                let limit = integer(&args, "limit", 32, 1, 128)? as usize;
                let tools: Vec<_> = self
                    .context
                    .tools
                    .iter()
                    .filter(|t| !self.ancestors.contains(&t.schema().name))
                    .collect();
                if tools.len() > 4096 {
                    return Err("tool catalog exceeds 4096 entries".into());
                }
                // Bound the complete catalog as well as each page.
                let all:Vec<_> = tools.iter().map(|t|json!({"name":t.schema().name,"description":t.schema().description,"parameters":t.schema().parameters})).collect();
                bounded(&json!(all))?;
                let end = offset.saturating_add(limit).min(all.len());
                let page = all.get(offset..end).unwrap_or(&[]);
                Ok(
                    json!({"protocolVersion":1,"capabilities":{"resultEnvelope":1,"hostAccounting":1,"parentProgress":1,"losslessValues":1,"resultReferences":1,"contentAttachments":1},"tools":page,"nextOffset":if end<all.len(){Some(end)}else{None}}),
                )
            }
            "read_result" => self.read_result(&args),
            "measure_result_content" => self.measure_result_content(&args),
            "release_result" => self.release_result(&args),
            "start" => {
                let reference = match args.get("resultMode") {
                    None => false,
                    Some(Value::String(mode)) if mode == "reference" => true,
                    _ => return Err("resultMode must be reference when provided".into()),
                };
                // The executing parent cannot hold its children's gate. It
                // must instead exclude peers in its own containing batch.
                // Inductively, this also orders independent program groups.
                if !self.ordered {
                    return Err(
                        "ToolProgram start requires a Sequential executing tool or loop".into(),
                    );
                }
                let name = args["name"].as_str().ok_or("name is required")?.to_owned();
                if self.ancestors.contains(&name) || self.ancestors.len() >= 16 {
                    return Err("recursive/ancestor tool invocation is forbidden".into());
                }
                let tool = self
                    .context
                    .tools
                    .iter()
                    .find(|t| t.schema().name == name)
                    .ok_or("tool is not in the effective active subset")?;
                let sequential = self.config.tool_execution == ToolExecutionMode::Sequential
                    || tool.execution_mode() == ToolExecutionMode::Sequential;
                let arguments = args
                    .get("arguments")
                    .filter(|v| v.is_object())
                    .ok_or("arguments must be an object")?
                    .clone();
                let timeout = integer(&args, "timeoutMs", 30000, 1, 120000)?;
                let permit = self
                    .active
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| "ToolProgram active request limit exceeded")?;
                let tree_permit = self
                    .tree_active
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| "ToolProgram invocation-tree active request limit exceeded")?;
                let signal = self.signal.child_token();
                let id = format!("tp-{}", NEXT.fetch_add(1, Ordering::Relaxed));
                let ordinal;
                {
                    let mut state = self.state.lock().unwrap();
                    if state.requests.len() >= 16 {
                        return Err("ToolProgram retained/active request limit exceeded".into());
                    }
                    #[allow(deprecated)] // Keep the workspace Rust 1.88 MSRV.
                    let previous = self
                        .starts
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                            (n < 128).then_some(n + 1)
                        })
                        .map_err(|_| "ToolProgram start budget exceeded")?;
                    ordinal = previous + 1;
                    if self.signal.is_cancelled() || self.terminated.load(Ordering::SeqCst) {
                        return Err("parent invocation is no longer active".into());
                    }
                    state.requests.insert(
                        id.clone(),
                        Request {
                            signal: signal.clone(),
                            result: None,
                        },
                    );
                }
                let this = self.clone();
                let request_id = id.clone();
                let progress_name = name.clone();
                let updates = Arc::new(AtomicUsize::new(0));
                self.report(&id, &progress_name, "started", 0, &signal);
                tokio::spawn(async move {
                    let _permit = permit;
                    let _tree_permit = tree_permit;
                    // One deadline covers native work, callback retirement and writer close.
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
                    let update_gate = this
                        .observer
                        .as_ref()
                        .map(|_| Arc::new(UpdateGate::default()));
                    // Strong writer ownership survives the work future's unwind/drain.
                    let mut observation: Option<Arc<dyn ToolProgramObservation>> = None;
                    let observation_error = this
                        .observer
                        .as_ref()
                        .map(|_| Arc::new(Mutex::new(None::<String>)));
                    let publish = |result: Result<Value, String>| {
                        this.report(
                            &request_id,
                            &progress_name,
                            if result.is_ok() {
                                if result.as_ref().is_ok_and(|r| r["isError"] == true) {
                                    "failed"
                                } else {
                                    "completed"
                                }
                            } else {
                                "failed"
                            },
                            updates.load(Ordering::Relaxed),
                            &signal,
                        );
                        let reply = result.unwrap_or_else(
                            |error| json!({"requestId":request_id,"status":"failed","error":error}),
                        );
                        if let Some(request) =
                            this.state.lock().unwrap().requests.get_mut(&request_id)
                        {
                            request.result = Some(reply);
                        }
                    };
                    let (mut result, mut published) = {
                        let work = async {
                            // The parent never takes this lock: Sequential programs cannot self-deadlock.
                            let _write;
                            let _read;
                            if sequential {
                                _write = Some(this.serial.write().await);
                                _read = None;
                            } else {
                                _read = Some(this.serial.read().await);
                                _write = None;
                            }
                            if signal.is_cancelled() {
                                return Err("nested invocation cancelled".to_owned());
                            }
                            if let Some(observer) = &this.observer {
                                let child = ToolProgramChild {
                                    root_id: this.identity.root_id.clone(),
                                    root_name: this.identity.root_name.clone(),
                                    parent_id: this.identity.parent_id.clone(),
                                    request_id: request_id.clone(),
                                    name: name.clone(),
                                    ordinal,
                                };
                                let writer = observer
                                    .begin(child, &arguments, signal.clone())
                                    .await
                                    .inspect_err(|_| this.cancel_all())?;
                                observation = Some(Arc::new(NativeObservation {
                                    writer,
                                    program: Arc::downgrade(&this),
                                    error: observation_error.as_ref().unwrap().clone(),
                                }));
                                if signal.is_cancelled() {
                                    return Err("nested invocation cancelled".into());
                                }
                            }
                            let call = ToolCall {
                                kind: ToolCallType,
                                id: request_id.clone(),
                                name,
                                arguments,
                                thought_signature: None,
                                namespace: None,
                                recorded: None,
                            };
                            let mut config = this.config.clone();
                            config.signal = signal.clone();
                            let emit: Arc<dyn AgentEmitter> = Arc::new(ProgressEmitter {
                                program: Arc::downgrade(&this),
                                request_id: request_id.clone(),
                                name: progress_name.clone(),
                                signal: signal.clone(),
                                updates: updates.clone(),
                                observation: observation.as_ref().map(Arc::downgrade),
                                observation_error: observation_error.clone(),
                                update_gate: update_gate.clone(),
                            });
                            let execute = PARENT.scope(
                                this.identity.clone(),
                                scope(
                                    this.clone(),
                                    crate::agent_loop::run_one_tool_call(
                                        &this.context,
                                        &this.assistant,
                                        &call,
                                        &config,
                                        &emit,
                                    ),
                                ),
                            );
                            let execute = async {
                                if let Some(scope) = &this.child_scope {
                                    scope.run(execute).await
                                } else {
                                    execute.await
                                }
                            };
                            let result = if let Some(observation) = &observation {
                                tool_program_observation::scope(observation.clone(), execute).await
                            } else {
                                execute.await
                            };
                            let finalized = result.map_err(|e| e.to_string())?;
                            let result = &finalized.result;
                            // Exactly one retention per direct request, before any fallible frame encoding.
                            {
                                let mut state = this.state.lock().unwrap();
                                state.accounting.retain(result);
                                // Serialize terminal retention with start's last admission check.
                                // Finalization, not fallible reply encoding, decides admission.
                                if result.terminate {
                                    this.terminated.store(true, Ordering::SeqCst);
                                }
                            }
                            if let Some(gate) = &update_gate {
                                gate.retire().await;
                            }
                            if let Some(observation) = &observation {
                                observation
                                    .finish(result, finalized.is_error, signal.is_cancelled())
                                    .await
                                    .inspect_err(|_| this.cancel_all())?;
                            }
                            if let Some(error) = observation_error
                                .as_ref()
                                .and_then(|e| e.lock().unwrap().clone())
                            {
                                return Err(error);
                            }
                            let reply = if reference {
                                let encoded =
                                    serde_json::to_string(result).map_err(|e| e.to_string())?;
                                this.retain_result(
                                    &request_id,
                                    finalized.result,
                                    finalized.is_error,
                                    encoded,
                                )?
                            } else {
                                let payload =
                                    serde_json::to_value(result).map_err(|e| e.to_string())?;
                                let reply = json!({"requestId":request_id,"status":"done","isError":finalized.is_error,"result":payload});
                                bounded(&reply)?;
                                reply
                            };
                            Ok::<_, String>(reply)
                        };
                        let work = std::panic::AssertUnwindSafe(work).catch_unwind();
                        tokio::pin!(work);
                        let (result, drain) = tokio::select! {
                            biased;
                            _ = signal.cancelled() => (Err("nested invocation cancelled".into()), true),
                            _ = tokio::time::sleep_until(deadline) => (
                                Err("nested invocation timed out; side effects may have occurred".into()), true),
                            result = &mut work => (match result {
                                Ok(reply) => reply,
                                Err(_) => {
                                    if this.observer.is_some() { this.cancel_all(); }
                                    Err("nested invocation panicked".into())
                                },
                            }, false),
                        };
                        if drain {
                            // Publish promptly, but retain native execution and permits.
                            publish(result.clone());
                            signal.cancel();
                            if (&mut work).await.is_err() && this.observer.is_some() {
                                // A writer may panic only after cancellation won
                                // the select. It still fails program admission closed.
                                this.cancel_all();
                            }
                        }
                        (result, drain)
                    };
                    let reason = result.as_ref().err().cloned();
                    // Own exactly one cleanup future. Cancellation/deadline may publish
                    // failure, but never drop retirement/close or release either permit.
                    let cleanup = async {
                        if let Some(gate) = &update_gate {
                            gate.retire().await;
                        }
                        // Native callbacks have retired, including their root reports.
                        if let Some(observation) = observation {
                            std::panic::AssertUnwindSafe(async {
                                observation.close(reason.as_deref()).await
                            })
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| Err("observation close panicked".into()))
                        } else {
                            Ok(())
                        }
                    };
                    tokio::pin!(cleanup);
                    let closed = if published {
                        (&mut cleanup).await
                    } else {
                        let interruption = tokio::select! {
                            biased;
                            _ = signal.cancelled() => Some("nested invocation cancelled"),
                            _ = tokio::time::sleep_until(deadline) => Some(
                                "nested invocation timed out; side effects may have occurred"),
                            closed = &mut cleanup => {
                                if let Err(error) = closed {
                                    this.cancel_all();
                                    result = Err(error);
                                }
                                None
                            }
                        };
                        if let Some(error) = interruption {
                            publish(Err(error.into()));
                            published = true;
                            signal.cancel();
                            (&mut cleanup).await
                        } else {
                            Ok(())
                        }
                    };
                    if let Err(error) = closed {
                        this.cancel_all();
                        result = Err(error);
                    }
                    if !published {
                        publish(result);
                    }
                    signal.cancel();
                });
                Ok(json!({"requestId":id,"status":"running"}))
            }
            "poll" | "cancel" | "dispose" => {
                let id = args["requestId"].as_str().ok_or("requestId is required")?;
                let mut state = self.state.lock().unwrap();
                if op == "dispose" {
                    if let Some(r) = state.requests.remove(id) {
                        r.signal.cancel();
                    }
                    return Ok(json!({"requestId":id,"status":"disposed"}));
                }
                let request = state
                    .requests
                    .get_mut(id)
                    .ok_or("unknown request for this invocation")?;
                if op == "cancel" {
                    request.signal.cancel();
                    Ok(json!({"requestId":id,"status":"cancelled"}))
                } else {
                    Ok(request
                        .result
                        .clone()
                        .unwrap_or_else(|| json!({"requestId":id,"status":"running"})))
                }
            }
            _ => Err("unknown ToolProgram operation".into()),
        }
    }
}
fn bounded(v: &Value) -> Result<(), String> {
    if serde_json::to_vec(v).map_err(|e| e.to_string())?.len() > MAX_BYTES {
        Err("ToolProgram JSON exceeds 1 MiB".into())
    } else {
        Ok(())
    }
}
fn integer(args: &Value, key: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    let value = match args.get(key) {
        None => default,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("{key} must be an integer"))?,
    };
    if !(min..=max).contains(&value) {
        Err(format!("{key} must be between {min} and {max}"))
    } else {
        Ok(value)
    }
}
// A prepared-barrier failure is an observation failure, not merely a tool
// error: stop admission in the genuine containing program. No strong registry
// reference is retained by the observation or by native callbacks.
struct NativeObservation {
    writer: Arc<dyn ToolProgramObservation>,
    program: Weak<ToolProgram>,
    error: Arc<Mutex<Option<String>>>,
}
impl ToolProgramObservation for NativeObservation {
    fn id(&self) -> &str {
        self.writer.id()
    }
    fn update(&self, partial: Arc<crate::AgentToolResult>) -> Result<(), String> {
        self.writer.update(partial)
    }
    fn prepared<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // Construct the writer future inside the catch as well as polling it.
            let result =
                std::panic::AssertUnwindSafe(async { self.writer.prepared(arguments).await })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err("observation prepared panicked".into()));
            if let Err(error) = &result {
                *self.error.lock().unwrap() = Some(error.clone());
                if let Some(program) = self.program.upgrade() {
                    program.cancel_all();
                }
            }
            result
        })
    }
    fn finish<'a>(
        &'a self,
        result: &'a crate::AgentToolResult,
        is_error: bool,
        cancelled: bool,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.writer.finish(result, is_error, cancelled)
    }
    fn close<'a>(&'a self, reason: Option<&'a str>) -> BoxFuture<'a, Result<(), String>> {
        self.writer.close(reason)
    }
}

// A single atomic orders admission against retirement: once CLOSED is set,
// no callback can increment the active count. Only observed requests allocate it.
#[derive(Default)]
struct UpdateGate {
    state: AtomicUsize,
    idle: Notify,
}
const UPDATES_CLOSED: usize = 1usize << (usize::BITS - 1);
impl UpdateGate {
    fn enter(&self) -> Option<UpdatePermit<'_>> {
        #[allow(deprecated)] // Keep the workspace Rust 1.88 MSRV.
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & UPDATES_CLOSED == 0 && state < UPDATES_CLOSED - 1).then(|| state + 1)
            })
            .ok()
            .map(|_| UpdatePermit(self))
    }
    async fn retire(&self) {
        self.state.fetch_or(UPDATES_CLOSED, Ordering::AcqRel);
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            // Register before checking the counter, so the last exiting callback
            // cannot notify between the check and waiter registration.
            idle.as_mut().enable();
            if self.state.load(Ordering::Acquire) == UPDATES_CLOSED {
                return;
            }
            idle.await;
        }
    }
}
struct UpdatePermit<'a>(&'a UpdateGate);
impl Drop for UpdatePermit<'_> {
    fn drop(&mut self) {
        if self.0.state.fetch_sub(1, Ordering::AcqRel) & !UPDATES_CLOSED == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

struct ProgressEmitter {
    // Native tools may retain their update callback in the program's tool registry.
    // Such callbacks must not keep that registry (and themselves) alive.
    program: Weak<ToolProgram>,
    request_id: String,
    name: String,
    signal: CancellationToken,
    updates: Arc<AtomicUsize>,
    observation: Option<Weak<dyn ToolProgramObservation>>,
    observation_error: Option<Arc<Mutex<Option<String>>>>,
    update_gate: Option<Arc<UpdateGate>>,
}
impl AgentEmitter for ProgressEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        self.try_emit(event);
        Box::pin(async {})
    }
    fn try_emit(&self, event: AgentEvent) {
        // Saved callbacks own only this standalone gate and weak runtime/writer
        // references. Hold admission through writer acceptance AND root reporting.
        let _admission = match &self.update_gate {
            Some(gate) => match gate.enter() {
                Some(permit) => Some(permit),
                None => return,
            },
            None => None,
        };
        if let (AgentEvent::ToolExecutionUpdate { partial_result, .. }, Some(program)) =
            (event, self.program.upgrade())
        {
            if !self.signal.is_cancelled() && !program.signal.is_cancelled() {
                if let Some(observation) = self.observation.as_ref().and_then(Weak::upgrade) {
                    let updated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        observation.update(partial_result)
                    }))
                    .unwrap_or_else(|_| Err("observation update panicked".into()));
                    if let Err(error) = updated {
                        if let Some(slot) = &self.observation_error {
                            *slot.lock().unwrap() = Some(error);
                        }
                        program.cancel_all();
                    }
                }
            }
            let count = self
                .updates
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            program.report(
                &self.request_id,
                &self.name,
                "progress",
                count,
                &self.signal,
            );
        }
    }
}

/// Only lifecycle identity and a count cross the outer native update callback.
struct Progress {
    callback: Arc<dyn Fn(crate::ToolResultPartial) + Send + Sync>,
    signal: CancellationToken,
    budget: Mutex<(usize, usize)>,
}
impl ToolProgram {
    fn report(&self, id: &str, name: &str, phase: &str, count: usize, signal: &CancellationToken) {
        if signal.is_cancelled()
            || self.signal.is_cancelled()
            || self.progress.signal.is_cancelled()
        {
            return;
        }
        let short = |s: &str| s.chars().take(256).collect::<String>();
        let details = json!({"toolProgram": {"rootId":short(&self.identity.root_id),
            "parentId":short(&self.identity.parent_id), "requestId":short(id),
            "tool":short(name), "phase":phase, "updateCount":count}});
        let bytes = details.to_string().len();
        {
            let mut budget = self.progress.budget.lock().unwrap();
            if budget.0 >= 256 || budget.1 + bytes > 64 * 1024 {
                return;
            }
            budget.0 += 1;
            budget.1 += bytes;
        }
        // An observer must never unwind past a cancelled worker's drain/permits.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.progress.callback)(crate::AgentToolResult {
                details,
                ..Default::default()
            });
        }));
    }
}
