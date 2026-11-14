//! Durable native child observations. No guest identity, replay, or transcript synthesis.
//!
//! Limits bound serialization staging and retained queue bytes, not process RSS.
//! Normal shutdown drains and joins. Dropping an unclosed driver aborts its owned
//! worker (there is no async detached cleanup); an interrupted append may leave
//! orphan bodies or an uncertain commit and is NEVER retried.
use std::{
    collections::HashMap,
    io::{self, Write},
    sync::{Arc, Mutex as SyncMutex},
};

use futures::future::BoxFuture;
use rpi_agent::{
    tool_program_observation::{ToolProgramChild, ToolProgramObservation, ToolProgramObserver},
    AgentToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    task::{AbortHandle, JoinHandle},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    session::{session::NativeJournalCommit, types::*, Session},
    tool_program_body::{append_body, BodyDescriptor, BodyKind, MAX_BODY_BYTES},
};

pub(crate) const BEGIN_CUSTOM_TYPE: &str = "rpi.tool_program.child_begin.v1";
pub(crate) const PHASE_CUSTOM_TYPE: &str = "rpi.tool_program.child_phase.v1";
pub(crate) fn is_reserved_custom_type(custom_type: &str) -> bool {
    matches!(custom_type, BEGIN_CUSTOM_TYPE | PHASE_CUSTOM_TYPE)
}
const MAX_MAPPINGS: usize = 128;
const QUEUE_COUNT: usize = 8;
const QUEUE_BYTES: usize = 8 * 1024 * 1024;
const METADATA_BYTES: usize = 16 * 1024;

type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ChildOrigin {
    pub run_id: String,
    pub assistant_entry_id: String,
    pub tool_index: u32,
}
pub(crate) type Correlate =
    Arc<dyn Fn(ToolProgramChild) -> BoxFuture<'static, Result<ChildOrigin>> + Send + Sync>;
pub(crate) type Fail = Arc<dyn Fn(String) + Send + Sync>;

/// Bounded writer DTO. Readers must also validate field/encoded-size bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Begin {
    pub version: u8,
    pub child_id: String,
    pub origin: ChildOrigin,
    pub root_call_id: String,
    pub root_tool_name: String,
    pub tool_name: String,
    pub ordinal: u32,
    pub parent_child_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PhaseKind {
    Requested,
    Prepared,
    Update,
    Final,
    Closed,
}

/// Only descriptor references, never arguments/results or private request IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Phase {
    pub version: u8,
    pub child_id: String,
    pub index: u64,
    pub phase: PhaseKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<BodyDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<bool>,
}

struct Counter {
    bytes: usize,
    max: usize,
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.max.saturating_sub(self.bytes) {
            return Err(io::Error::other("journal serialization quota exceeded"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn size<T: Serialize + ?Sized>(value: &T, max: usize) -> Result<usize> {
    let mut sink = Counter { bytes: 0, max };
    serde_json::to_writer(&mut sink, value)
        .map_err(|_| "journal serialization quota exceeded".to_owned())?;
    Ok(sink.bytes)
}
fn bounded(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err("invalid journal identity".into());
    }
    Ok(())
}

struct Mapping {
    begin: Begin,
}
#[derive(Default)]
struct Transaction {
    anchor: Option<String>,
    // Active private identities only. Native IDs are globally minted; tombstones
    // would impose a new run-wide start budget across independent programs.
    // Never serialized, emitted, or included in errors.
    children: HashMap<String, Mapping>,
}
struct Core {
    session: Session,
    lane: String,
    transaction: Mutex<Transaction>,
    error: SyncMutex<Option<String>>,
    fail: Fail,
}
impl Core {
    fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
    fn fail(&self, message: &str) -> String {
        let mut error = self.error.lock().unwrap();
        if let Some(error) = error.as_ref() {
            return error.clone();
        }
        let message = message.to_owned();
        *error = Some(message.clone());
        drop(error);
        (self.fail)(message.clone());
        message
    }
    fn available(&self) -> Result<()> {
        match self.error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    async fn leaf(&self) -> Result<String> {
        self.session
            .get_lanes()
            .await
            .map_err(|_| "journal lane read failed".to_owned())?
            .into_iter()
            .find(|l| l.lane == self.lane)
            .and_then(|l| l.leaf_id)
            .ok_or_else(|| "journal lane has no anchor".into())
    }
    async fn member(&self, leaf: &str, id: &str) -> Result<()> {
        let entries = self
            .session
            .storage()
            .find_entries_on_branch(
                &EntryQuery {
                    entry_id: Some(id.to_owned()),
                    limit: Some(1),
                    ..Default::default()
                },
                &BranchBounds::default(),
                leaf,
            )
            .await
            .map_err(|_| "journal branch validation failed".to_owned())?;
        if entries.len() != 1 || entries[0].id() != id {
            return Err("journal anchor is not on current branch".into());
        }
        Ok(())
    }
    async fn validate_anchor(&self, tx: &Transaction) -> Result<String> {
        let leaf = self.leaf().await?;
        if let Some(anchor) = &tx.anchor {
            self.member(&leaf, anchor).await?;
        }
        Ok(leaf)
    }
    async fn append<T: Serialize>(
        &self,
        tx: &mut Transaction,
        entry_id: String,
        custom_type: &str,
        data: &T,
    ) -> Result<()> {
        self.session
            .with_mutation(|_| async move {
                self.append_unlocked(tx, entry_id, custom_type, data).await
            })
            .await
    }
    async fn append_unlocked<T: Serialize>(
        &self,
        tx: &mut Transaction,
        entry_id: String,
        custom_type: &str,
        data: &T,
    ) -> Result<()> {
        size(data, METADATA_BYTES)?;
        let canonical_data = serde_json::to_value(data)
            .map_err(|_| "journal metadata encoding failed".to_owned())?;
        // Pin the source identity before any revision/ancestry check or append.
        // The commit signal must never discover a session id after a facade
        // switch, and a metadata read failure must prevent the append attempt.
        let source_session_id = self
            .session
            .get_metadata()
            .await
            .map_err(|_| "journal metadata read failed".to_owned())?
            .id;
        bounded(&source_session_id)?;
        // Read the revision BEFORE the ancestry validation. Mutation during either
        // check fails the native conditional append. Never fall back or retry.
        let revision = self
            .session
            .storage()
            .history_revision()
            .await
            .map_err(|_| "journal revision read failed".to_owned())?;
        self.validate_anchor(tx).await?;
        let entry = ProvisionedEntry {
            id: entry_id.clone(),
            kind: ProvisionedKind::Custom {
                custom_type: custom_type.into(),
                data: Some(canonical_data.clone()),
            },
        };
        let stamped = self
            .session
            .append_entry_if_revision(entry, &self.lane, revision)
            .await
            .map_err(|_| "journal conditional append failed".to_owned())?;
        // This is deliberately synchronous and follows the real append ACK. The
        // stamped native seq/parent are the durable ordering/reconciliation key.
        self.session
            .publish_tool_program_commit(NativeJournalCommit {
                schema_version: 1,
                source_session_id,
                lane: self.lane.clone(),
                entry_id: stamped.base().id.clone(),
                parent_entry_id: stamped.base().parent_id.clone(),
                seq: stamped.base().seq,
                custom_type: custom_type.to_owned(),
                data: canonical_data,
            });
        tx.anchor = Some(entry_id);
        Ok(())
    }
    async fn body<T: Serialize + Sync>(
        &self,
        tx: &mut Transaction,
        child_id: &str,
        kind: BodyKind,
        value: &T,
    ) -> Result<BodyDescriptor> {
        self.validate_anchor(tx).await?;
        let descriptor = append_body(
            &self.session,
            &self.lane,
            child_id,
            kind,
            value,
            MAX_BODY_BYTES,
        )
        .await
        .map_err(|_| "journal body persistence failed".to_owned())?;
        // The body primitive owns its per-chunk CAS. Also prove that its new
        // commit did not escape our preceding journal anchor between awaits.
        if let Some(anchor) = &tx.anchor {
            self.member(&descriptor.descriptor_id, anchor).await?;
        }
        tx.anchor = Some(descriptor.descriptor_id.clone());
        Ok(descriptor)
    }
    async fn phase(&self, tx: &mut Transaction, phase: &Phase) -> Result<()> {
        self.append(
            tx,
            format!("tpphase-{}", Uuid::new_v4()),
            PHASE_CUSTOM_TYPE,
            phase,
        )
        .await
    }
}

/// Cancellation during a borrowed storage transaction is a sticky run failure,
/// not permission to retry an append whose ACK may have been lost.
struct Attempt {
    core: Arc<Core>,
    armed: bool,
}
impl Attempt {
    fn new(core: &Arc<Core>) -> Self {
        Self {
            core: core.clone(),
            armed: true,
        }
    }
    fn complete<T>(mut self, result: Result<T>) -> Result<T> {
        self.armed = false;
        result.map_err(|e| self.core.fail(&e))
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if self.armed {
            self.core.fail("child journal acknowledgement interrupted");
        }
    }
}

pub(crate) struct JournalObserver {
    core: Arc<Core>,
    correlate: Correlate,
}
impl JournalObserver {
    pub(crate) fn new(session: Session, lane: String, correlate: Correlate, fail: Fail) -> Self {
        Self {
            core: Arc::new(Core {
                session: session.pin(session.id_generator().clone()),
                lane,
                transaction: Mutex::new(Transaction::default()),
                error: SyncMutex::new(None),
                fail,
            }),
            correlate,
        }
    }
    async fn begin_inner(
        &self,
        child: ToolProgramChild,
        arguments: &Value,
    ) -> Result<Arc<dyn ToolProgramObservation>> {
        self.core.available()?;
        bounded(&self.core.lane)?;
        for field in [
            &child.root_id,
            &child.root_name,
            &child.parent_id,
            &child.request_id,
            &child.name,
        ] {
            bounded(field)?;
        }
        if child.root_id == child.request_id {
            return Err("child cannot be its own root".into());
        }
        let ordinal =
            u32::try_from(child.ordinal).map_err(|_| "invalid child ordinal".to_owned())?;
        let origin = (self.correlate)(child.clone())
            .await
            .map_err(|_| "native child correlation failed".to_owned())?;
        bounded(&origin.run_id)?;
        bounded(&origin.assistant_entry_id)?;
        let mut tx = self.core.transaction.lock().await;
        self.core.available()?;
        if tx.children.len() >= MAX_MAPPINGS || tx.children.contains_key(&child.request_id) {
            return Err("duplicate active child identity or mapping quota exceeded".into());
        }
        let leaf = self.core.validate_anchor(&tx).await?;
        self.core.member(&leaf, &origin.assistant_entry_id).await?;
        if tx.children.values().any(|mapping| {
            mapping.begin.root_call_id == child.root_id
                && (mapping.begin.origin != origin
                    || mapping.begin.root_tool_name != child.root_name)
        }) {
            return Err("inconsistent native root origin".into());
        }
        // Preserve the exact branch on which the assistant and mapped parent
        // were authenticated, including the very first begin.
        tx.anchor = Some(leaf.clone());
        let parent_child_id = if child.parent_id == child.root_id {
            None
        } else {
            let parent = tx
                .children
                .get(&child.parent_id)
                .ok_or_else(|| "missing native child parent".to_owned())?;
            if parent.begin.origin != origin
                || parent.begin.root_call_id != child.root_id
                || parent.begin.root_tool_name != child.root_name
            {
                return Err("foreign native child parent".into());
            }
            self.core.member(&leaf, &parent.begin.child_id).await?;
            Some(parent.begin.child_id.clone())
        };
        // Preflight the requested body BEFORE any begin append. The body primitive
        // performs its own canonical admission before appending chunks.
        size(arguments, MAX_BODY_BYTES)?;
        let begin = Begin {
            version: 1,
            child_id: format!("tpchild-{}", Uuid::new_v4()),
            origin,
            root_call_id: child.root_id,
            root_tool_name: child.root_name,
            tool_name: child.name,
            ordinal,
            parent_child_id,
        };
        self.core
            .append(&mut tx, begin.child_id.clone(), BEGIN_CUSTOM_TYPE, &begin)
            .await?;
        let descriptor = self
            .core
            .body(&mut tx, &begin.child_id, BodyKind::Requested, arguments)
            .await?;
        self.core
            .phase(
                &mut tx,
                &Phase {
                    version: 1,
                    child_id: begin.child_id.clone(),
                    index: 0,
                    phase: PhaseKind::Requested,
                    descriptor: Some(descriptor),
                    is_error: None,
                    cancelled: None,
                    failed: None,
                    interrupted: None,
                },
            )
            .await?;
        tx.children.insert(
            child.request_id,
            Mapping {
                begin: begin.clone(),
            },
        );
        drop(tx);
        // No worker or channel exists until ALL fallible begin storage setup ACKs.
        let observation = Observation::start(self.core.clone(), begin.child_id);
        Ok(Arc::new(observation))
    }
}
impl ToolProgramObserver for JournalObserver {
    fn begin<'a>(
        &'a self,
        child: ToolProgramChild,
        arguments: &'a Value,
        _signal: CancellationToken,
    ) -> BoxFuture<'a, Result<Arc<dyn ToolProgramObservation>>> {
        Box::pin(async move {
            let attempt = Attempt::new(&self.core);
            attempt.complete(self.begin_inner(child, arguments).await)
        })
    }
}

struct Admission {
    sender: Option<mpsc::Sender<Work>>,
    count: usize,
    bytes: usize,
    stopped: bool,
}
enum Work {
    Update {
        result: Arc<AgentToolResult>,
        bytes: usize,
    },
    // Worker pauses until the borrowed prepared/final transaction finishes. A
    // dropped release sender also unpauses it if that caller is cancelled.
    Barrier {
        arrived: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
}
struct Child {
    id: String,
    next: SyncMutex<u64>,
}
impl Child {
    fn phase(&self, phase: PhaseKind, descriptor: Option<BodyDescriptor>) -> Phase {
        let mut next = self.next.lock().unwrap();
        let index = *next;
        *next += 1;
        Phase {
            version: 1,
            child_id: self.id.clone(),
            index,
            phase,
            descriptor,
            is_error: None,
            cancelled: None,
            failed: None,
            interrupted: None,
        }
    }
}
struct Lifecycle {
    worker: Option<JoinHandle<()>>,
    prepared: bool,
    finalized: bool,
    final_committed: bool,
    close_result: Option<Result<()>>,
}
struct Observation {
    core: Arc<Core>,
    child: Arc<Child>,
    admission: Arc<SyncMutex<Admission>>,
    lifecycle: Mutex<Lifecycle>,
    abort: AbortHandle,
}
impl Observation {
    fn start(core: Arc<Core>, id: String) -> Self {
        // One spare slot for the single lifecycle barrier; payload reservations
        // independently cap accepted updates INCLUDING the in-flight update.
        let (sender, receiver) = mpsc::channel(QUEUE_COUNT + 1);
        let admission = Arc::new(SyncMutex::new(Admission {
            sender: Some(sender),
            count: 0,
            bytes: 0,
            stopped: false,
        }));
        let child = Arc::new(Child {
            id,
            next: SyncMutex::new(1),
        });
        let worker = tokio::spawn(Self::worker(
            core.clone(),
            child.clone(),
            admission.clone(),
            receiver,
        ));
        let abort = worker.abort_handle();
        Self {
            core,
            child,
            admission,
            abort,
            lifecycle: Mutex::new(Lifecycle {
                worker: Some(worker),
                prepared: false,
                finalized: false,
                final_committed: false,
                close_result: None,
            }),
        }
    }
    async fn worker(
        core: Arc<Core>,
        child: Arc<Child>,
        admission: Arc<SyncMutex<Admission>>,
        mut receiver: mpsc::Receiver<Work>,
    ) {
        while let Some(work) = receiver.recv().await {
            match work {
                Work::Update { result, bytes } => {
                    if core.available().is_ok() {
                        let mut tx = core.transaction.lock().await;
                        if core.available().is_ok() {
                            let write = async {
                                let descriptor = core
                                    .body(&mut tx, &child.id, BodyKind::Update, result.as_ref())
                                    .await?;
                                core.phase(
                                    &mut tx,
                                    &child.phase(PhaseKind::Update, Some(descriptor)),
                                )
                                .await
                            }
                            .await;
                            if let Err(e) = write {
                                core.fail(&e);
                            }
                        }
                    }
                    // Keep the Arc and reservation until its storage attempt completes.
                    drop(result);
                    let mut admission = admission.lock().unwrap();
                    admission.count -= 1;
                    admission.bytes -= bytes;
                }
                Work::Barrier { arrived, release } => {
                    let _ = arrived.send(());
                    let _ = release.await;
                }
            }
        }
    }
    async fn barrier(&self, stop: bool) -> Result<oneshot::Sender<()>> {
        let (arrived, wait) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        {
            let mut admission = self.admission.lock().unwrap();
            if stop {
                admission.stopped = true;
            }
            let sender = admission
                .sender
                .as_ref()
                .ok_or_else(|| "child writer is closed".to_owned())?;
            sender
                .try_send(Work::Barrier {
                    arrived,
                    release: resume,
                })
                .map_err(|_| "child barrier unavailable".to_owned())?;
        }
        wait.await
            .map_err(|_| "child worker unavailable".to_owned())?;
        Ok(release)
    }
    async fn write_body<T: Serialize + Sync>(
        &self,
        kind: BodyKind,
        phase: PhaseKind,
        value: &T,
        flags: Option<(bool, bool)>,
    ) -> Result<()> {
        let mut tx = self.core.transaction.lock().await;
        self.core.available()?;
        let descriptor = self.core.body(&mut tx, &self.child.id, kind, value).await?;
        let mut record = self.child.phase(phase, Some(descriptor));
        if let Some((is_error, cancelled)) = flags {
            record.is_error = Some(is_error);
            record.cancelled = Some(cancelled);
        }
        self.core.phase(&mut tx, &record).await
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.admission.lock().unwrap().sender.take();
        self.abort.abort();
    }
}
impl ToolProgramObservation for Observation {
    fn id(&self) -> &str {
        &self.child.id
    }
    fn update(&self, result: Arc<AgentToolResult>) -> Result<()> {
        let accepted = (|| {
            // This mutex establishes synchronous callback acceptance order.
            let mut admission = self.admission.lock().unwrap();
            self.core.available()?;
            if admission.stopped {
                return Err("child update admission closed".into());
            }
            if admission.count >= QUEUE_COUNT {
                return Err("child update count quota exceeded".into());
            }
            let bytes = size(result.as_ref(), QUEUE_BYTES.saturating_sub(admission.bytes))?;
            let sender = admission
                .sender
                .as_ref()
                .ok_or_else(|| "child writer is closed".to_owned())?;
            sender
                .try_send(Work::Update { result, bytes })
                .map_err(|_| "child update queue unavailable".to_owned())?;
            admission.count += 1;
            admission.bytes += bytes;
            Ok(())
        })();
        accepted.map_err(|e: String| self.core.fail(&e))
    }
    fn prepared<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let attempt = Attempt::new(&self.core);
            let result = async {
                let mut lifecycle = self.lifecycle.lock().await;
                if lifecycle.prepared || lifecycle.finalized || lifecycle.close_result.is_some() {
                    return Err("child prepared phase already retired".into());
                }
                lifecycle.prepared = true; // no retry of uncertain/cancelled write
                let _release = self.barrier(false).await?;
                self.write_body(BodyKind::Prepared, PhaseKind::Prepared, arguments, None)
                    .await
            }
            .await;
            attempt.complete(result)
        })
    }
    fn finish<'a>(
        &'a self,
        result: &'a AgentToolResult,
        is_error: bool,
        cancelled: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let attempt = Attempt::new(&self.core);
            let write = async {
                let mut lifecycle = self.lifecycle.lock().await;
                if lifecycle.finalized || lifecycle.close_result.is_some() {
                    return Err("child final phase already retired".into());
                }
                lifecycle.finalized = true;
                let _release = self.barrier(true).await?;
                self.write_body(
                    BodyKind::Final,
                    PhaseKind::Final,
                    result,
                    Some((is_error, cancelled)),
                )
                .await?;
                lifecycle.final_committed = true;
                Ok(())
            }
            .await;
            attempt.complete(write)
        })
    }
    fn close<'a>(&'a self, reason: Option<&'a str>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut lifecycle = self.lifecycle.lock().await;
            if let Some(result) = &lifecycle.close_result {
                return result.clone();
            }
            let attempt = Attempt::new(&self.core);
            {
                let mut admission = self.admission.lock().unwrap();
                admission.stopped = true;
                admission.sender.take();
            }
            // Await IN PLACE: cancellation of close does not detach the join handle.
            if let Some(worker) = lifecycle.worker.as_mut() {
                if worker.await.is_err() {
                    self.core.fail("child worker join failed");
                }
            }
            lifecycle.worker.take();
            // An interrupted close append is uncertain. Mark attempted before the
            // await so a later close cannot retry it or fabricate another record.
            lifecycle.close_result = Some(Err("child close acknowledgement interrupted".into()));
            let mut tx = self.core.transaction.lock().await;
            let mut record = self.child.phase(PhaseKind::Closed, None);
            record.failed = Some(self.core.error().is_some() || reason.is_some());
            record.interrupted = Some(reason.is_some() || !lifecycle.final_committed);
            // Best-effort NEW closed record, even after an earlier body failure;
            // never replay the failed body/phase. No raw error/private ID stored.
            if let Err(e) = self.core.phase(&mut tx, &record).await {
                self.core.fail(&e);
            }
            tx.children
                .retain(|_, mapping| mapping.begin.child_id != self.child.id);
            let result = self.core.available();
            lifecycle.close_result = Some(result.clone());
            attempt.complete(result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        session::{
            session::{SessionEntryCommit, NATIVE_JOURNAL_EVENT_MAX_BYTES},
            DefaultIdGenerator, InMemorySessionStorage, SystemClock,
        },
        tool_program_body::{read_body_page, BodyPageQuery, MAX_PAGE_BYTES},
    };
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{timeout, Duration};

    fn memory(name: &str) -> Session {
        Session::new(
            Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: name.into(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )),
            None,
        )
    }
    // The parent owns full assistant/tool-call correlation. This fixture gives
    // its trusted closure a native anchor, not an identity from argument JSON.
    async fn fixture() -> (Session, JournalObserver, Arc<AtomicUsize>) {
        let (session, observer, failures, _) = fixture_with_publisher().await;
        (session, observer, failures)
    }
    async fn fixture_with_publisher() -> (
        Session,
        JournalObserver,
        Arc<AtomicUsize>,
        tokio::sync::broadcast::Receiver<SessionEntryCommit>,
    ) {
        let session = memory("journal");
        let assistant = session
            .append_message(rpi_agent::AgentMessage::Assistant(Box::new(
                rpi_ai::types::AssistantMessage::empty(
                    rpi_ai::types::Api::OpenaiResponses,
                    "test",
                    "test",
                    0,
                ),
            )))
            .await
            .unwrap();
        let (publisher, commits) = tokio::sync::broadcast::channel::<SessionEntryCommit>(256);
        session.set_entry_commit_publisher(Some(publisher));
        let origin = ChildOrigin {
            run_id: "run-native".into(),
            assistant_entry_id: assistant,
            tool_index: 2,
        };
        let correlate: Correlate = Arc::new(move |_| {
            let origin = origin.clone();
            Box::pin(async move { Ok(origin) })
        });
        let failures = Arc::new(AtomicUsize::new(0));
        let count = failures.clone();
        let observer = JournalObserver::new(
            session.clone(),
            "main".into(),
            correlate,
            Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        );
        (session, observer, failures, commits)
    }
    fn child(request: &str, parent: &str) -> ToolProgramChild {
        ToolProgramChild {
            root_id: "root-call-native".into(),
            root_name: "tool_program".into(),
            parent_id: parent.into(),
            request_id: request.into(),
            name: "read".into(),
            ordinal: 1,
        }
    }
    async fn begin(observer: &JournalObserver, request: &str) -> Arc<dyn ToolProgramObservation> {
        observer
            .begin(
                child(request, "root-call-native"),
                &json!({"path":"requested"}),
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }
    async fn records(session: &Session) -> (Vec<Begin>, Vec<Phase>) {
        let entries = session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut begins = vec![];
        let mut phases = vec![];
        for entry in entries {
            if let Entry::Custom(custom) = entry {
                if custom.custom_type == BEGIN_CUSTOM_TYPE {
                    assert!(size(&custom.data, METADATA_BYTES).is_ok());
                    begins.push(serde_json::from_value(custom.data.unwrap()).unwrap());
                } else if custom.custom_type == PHASE_CUSTOM_TYPE {
                    assert!(size(&custom.data, METADATA_BYTES).is_ok());
                    phases.push(serde_json::from_value(custom.data.unwrap()).unwrap());
                }
            }
        }
        (begins, phases)
    }
    async fn body(session: &Session, descriptor: &BodyDescriptor) -> Value {
        let mut query = BodyPageQuery {
            descriptor_id: descriptor.descriptor_id.clone(),
            leaf_id: None,
            cursor: None,
            limit: 8,
            max_bytes: MAX_PAGE_BYTES,
        };
        let mut bytes = vec![];
        loop {
            let page = read_body_page(session, "main", &query).await.unwrap();
            for chunk in page.chunks {
                bytes.extend(STANDARD.decode(chunk.data).unwrap());
            }
            query.cursor = page.next_cursor;
            if query.cursor.is_none() {
                break;
            }
        }
        serde_json::from_slice(&bytes).unwrap()
    }
    fn signed(text: &str) -> AgentToolResult {
        let mut result = AgentToolResult::text(text);
        if let rpi_agent::TextContentOrImage::Text(t) = &mut result.content[0] {
            t.text_signature = Some("provider-native-signature".into());
        }
        result.details = json!({"exact":"é🦀", "signature":"detail-signature"});
        result
    }

    #[tokio::test]
    async fn durable_requested_prepared_ordered_signed_updates_posthook_final_closed() {
        let (session, observer, failures) = fixture().await;
        let observation = begin(&observer, "tp-private-first").await;
        assert!(observation.id().starts_with("tpchild-"));
        let id = Uuid::parse_str(observation.id().strip_prefix("tpchild-").unwrap()).unwrap();
        assert_eq!(id.get_version_num(), 4);
        let (_, phases) = records(&session).await;
        assert_eq!(phases.len(), 1); // begin/requested ACK before writer publication
        assert_eq!(
            body(&session, phases[0].descriptor.as_ref().unwrap()).await,
            json!({"path":"requested"})
        );
        observation
            .prepared(&json!({"path":"native-prepared"}))
            .await
            .unwrap();
        let a = Arc::new(signed("partial one"));
        let b = Arc::new(signed("partial two"));
        observation.update(a.clone()).unwrap();
        observation.update(b.clone()).unwrap();
        let mut posthook = signed("changed by native after-tool hook");
        posthook.terminate = true;
        posthook.added_tool_names.push("new-native-tool".into());
        observation.finish(&posthook, true, false).await.unwrap();
        let (c1, c2) = tokio::join!(
            observation.close(None),
            observation.close(Some("ignored concurrent reason"))
        );
        assert_eq!(c1, c2);
        c1.unwrap();
        let (begins, phases) = records(&session).await;
        assert_eq!(begins.len(), 1);
        assert_eq!(begins[0].origin.tool_index, 2);
        assert_eq!(
            phases.iter().map(|p| p.phase).collect::<Vec<_>>(),
            vec![
                PhaseKind::Requested,
                PhaseKind::Prepared,
                PhaseKind::Update,
                PhaseKind::Update,
                PhaseKind::Final,
                PhaseKind::Closed
            ]
        );
        assert_eq!(
            phases.iter().map(|p| p.index).collect::<Vec<_>>(),
            (0..6).collect::<Vec<_>>()
        );
        assert_eq!(
            body(&session, phases[1].descriptor.as_ref().unwrap()).await,
            json!({"path":"native-prepared"})
        );
        for (index, expected) in [(2, a.as_ref()), (3, b.as_ref()), (4, &posthook)] {
            assert_eq!(
                body(&session, phases[index].descriptor.as_ref().unwrap()).await,
                serde_json::to_value(expected).unwrap()
            );
        }
        assert_eq!(phases[4].is_error, Some(true));
        assert_eq!(phases[4].cancelled, Some(false));
        assert_eq!(phases[5].failed, Some(false));
        assert_eq!(phases[5].interrupted, Some(false));
        assert_eq!(failures.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn native_commit_signals_follow_ack_order_and_pin_source_metadata() {
        let (session, observer, failures, mut commits) = fixture_with_publisher().await;
        let original = session.storage();

        let observation = begin(&observer, "tp-private-signal").await;
        observation
            .prepared(&json!({"prepared": true}))
            .await
            .unwrap();
        // Rebinding while the child is active must not change the source
        // identity or redirect its subsequent update/final/closed appends.
        session.set_storage(memory("replacement").storage());
        observation
            .update(Arc::new(signed("private-update")))
            .unwrap();
        observation
            .finish(&signed("private-result"), false, false)
            .await
            .unwrap();
        observation.close(None).await.unwrap();
        let captured: Vec<_> = std::iter::from_fn(|| commits.try_recv().ok()).collect();
        let native: Vec<_> = captured
            .iter()
            .filter_map(|commit| commit.native_journal.as_ref())
            .collect();
        assert_eq!(native.len(), 6); // begin + requested, prepared, update, final, closed
        assert!(native
            .windows(2)
            .all(|window| window[0].seq < window[1].seq));
        assert!(native
            .iter()
            .all(|commit| commit.source_session_id == "journal"));
        assert!(native.iter().all(|commit| {
            serde_json::to_vec(commit).unwrap().len() <= NATIVE_JOURNAL_EVENT_MAX_BYTES
        }));
        for commit in &native {
            let entry = original.get_entry(&commit.entry_id).await.unwrap().unwrap();
            match entry {
                Entry::Custom(custom) => assert_eq!(custom.data.unwrap(), commit.data),
                other => panic!("native signal points to non-custom entry: {other:?}"),
            }
            let encoded = serde_json::to_string(commit).unwrap();
            assert!(!encoded.contains("tp-private"));
            assert!(!encoded.contains("private-result"));
            assert!(!encoded.contains("private-update"));
            assert!(!encoded.contains("provider-native-signature"));
        }
        // Each native append still has exactly one legacy invalidation, while
        // the dedicated signal is an additional projection, not a duplicate.
        for commit in &native {
            assert_eq!(
                captured
                    .iter()
                    .filter(|candidate| candidate.native_journal.is_none()
                        && candidate.entry_id == commit.entry_id)
                    .count(),
                1
            );
        }
        assert_eq!(failures.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_native_append_emits_no_native_metadata() {
        let (session, observer, failures, mut commits) = fixture_with_publisher().await;
        let observation = begin(&observer, "tp-private-failed-signal").await;
        let before: Vec<_> = std::iter::from_fn(|| commits.try_recv().ok()).collect();
        let native_before = before
            .iter()
            .filter(|commit| commit.native_journal.is_some())
            .count();
        assert_eq!(native_before, 2); // begin/requested only

        // Rebase invalidates the captured anchor. The conditional append fails,
        // so no native metadata may be published for the failed phase.
        let assistant = observer
            .core
            .transaction
            .lock()
            .await
            .children
            .values()
            .next()
            .unwrap()
            .begin
            .origin
            .assistant_entry_id
            .clone();
        session.move_lane("main", Some(&assistant)).await.unwrap();
        assert!(observation
            .prepared(&json!({"will": "fail"}))
            .await
            .is_err());
        let after: Vec<_> = std::iter::from_fn(|| commits.try_recv().ok()).collect();
        assert!(after.iter().all(|commit| commit.native_journal.is_none()));
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        assert!(observation.close(None).await.is_err());
    }

    #[tokio::test]
    async fn retired_children_do_not_limit_later_independent_programs() {
        let (_, observer, failures) = fixture().await;
        for n in 0..130 {
            let mut request = child(&format!("tp-native-{n}"), "unused");
            request.root_id = format!("native-root-{n}");
            request.parent_id = request.root_id.clone();
            let observation = observer
                .begin(request, &json!({}), CancellationToken::new())
                .await
                .unwrap();
            observation.close(None).await.unwrap();
            assert!(observer.core.transaction.lock().await.children.is_empty());
        }
        assert_eq!(failures.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn nested_native_mapping_and_private_identity_never_persisted() {
        let (session, observer, _) = fixture().await;
        let parent = begin(&observer, "tp-private-parent").await;
        let nested = observer
            .begin(
                child("tp-private-nested", "tp-private-parent"),
                &json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        nested.close(None).await.unwrap();
        parent.close(None).await.unwrap();
        let (begins, phases) = records(&session).await;
        assert_eq!(begins[0].parent_child_id, None);
        assert_eq!(begins[1].parent_child_id.as_deref(), Some(parent.id()));
        assert_eq!(begins[1].origin, begins[0].origin);
        let metadata = serde_json::to_string(&(begins, phases)).unwrap();
        assert!(!metadata.contains("tp-private"));
    }

    #[tokio::test]
    async fn missing_duplicate_foreign_parent_and_inconsistent_origin_fail_sticky() {
        for mode in 0..4 {
            let (_session, mut observer, failures) = fixture().await;
            let parent = begin(&observer, "tp-private-parent").await;
            let mut request = child("tp-private-next", "tp-private-parent");
            match mode {
                0 => request.parent_id = "tp-missing".into(),
                1 => request.request_id = "tp-private-parent".into(),
                2 => request.root_id = "foreign-root-call".into(),
                _ => {
                    let mut changed = observer
                        .core
                        .transaction
                        .lock()
                        .await
                        .children
                        .values()
                        .next()
                        .unwrap()
                        .begin
                        .origin
                        .clone();
                    changed.run_id = "different-run".into();
                    observer.correlate = Arc::new(move |_| {
                        let changed = changed.clone();
                        Box::pin(async move { Ok(changed) })
                    });
                }
            }
            assert!(observer
                .begin(request, &json!({}), CancellationToken::new())
                .await
                .is_err());
            assert_eq!(failures.load(Ordering::SeqCst), 1);
            let error = parent.close(None).await.unwrap_err();
            assert_eq!(parent.close(None).await.unwrap_err(), error);
            assert!(!error.contains("tp-private"));
        }
    }

    #[tokio::test]
    async fn count_overflow_including_inflight_cancels_and_drains_all_arcs() {
        let (session, observer, failures) = fixture().await;
        let observation = begin(&observer, "tp-private-count").await;
        let tx = observer.core.transaction.lock().await;
        let mut retained = vec![];
        for i in 0..QUEUE_COUNT {
            let result = Arc::new(signed(&i.to_string()));
            retained.push(Arc::downgrade(&result));
            observation.update(result).unwrap();
            tokio::task::yield_now().await;
        }
        assert!(observation.update(Arc::new(signed("overflow"))).is_err());
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        drop(tx);
        assert!(timeout(Duration::from_secs(3), observation.close(None))
            .await
            .unwrap()
            .is_err());
        assert!(retained.iter().all(|w| w.upgrade().is_none()));
        let (_, phases) = records(&session).await;
        assert_eq!(phases.last().unwrap().phase, PhaseKind::Closed);
        assert_eq!(phases.last().unwrap().failed, Some(true));
        assert!(!phases.iter().any(|p| p.phase == PhaseKind::Final));
    }

    #[tokio::test]
    async fn encoded_bytes_overflow_and_individual_oversize_reject_before_enqueue() {
        for cumulative in [false, true] {
            let (_session, observer, failures) = fixture().await;
            let observation = begin(&observer, "tp-private-bytes").await;
            let tx = observer.core.transaction.lock().await;
            let mut retained = None;
            if cumulative {
                let first = Arc::new(signed(&"x".repeat(QUEUE_BYTES / 2)));
                retained = Some(Arc::downgrade(&first));
                observation.update(first).unwrap();
            }
            let oversize = Arc::new(signed(&"x".repeat(if cumulative {
                QUEUE_BYTES / 2
            } else {
                QUEUE_BYTES
            })));
            let rejected = Arc::downgrade(&oversize);
            assert!(observation.update(oversize).is_err());
            assert!(rejected.upgrade().is_none());
            assert_eq!(failures.load(Ordering::SeqCst), 1);
            drop(tx);
            assert!(observation.close(None).await.is_err());
            assert!(retained.is_none_or(|w| w.upgrade().is_none()));
        }
    }

    #[tokio::test]
    async fn final_has_separate_capacity_and_preserves_posthook_result() {
        let (session, observer, failures) = fixture().await;
        let observation = begin(&observer, "tp-private-final").await;
        let final_result = signed(&"z".repeat(QUEUE_BYTES + 1));
        assert!(size(&final_result, QUEUE_BYTES).is_err());
        assert!(size(&final_result, MAX_BODY_BYTES).is_ok());
        observation
            .finish(&final_result, false, true)
            .await
            .unwrap();
        observation.close(None).await.unwrap();
        let (_, phases) = records(&session).await;
        let final_phase = phases.iter().find(|p| p.phase == PhaseKind::Final).unwrap();
        assert_eq!(final_phase.cancelled, Some(true));
        assert_eq!(
            body(&session, final_phase.descriptor.as_ref().unwrap()).await,
            serde_json::to_value(&final_result).unwrap()
        );
        assert_eq!(failures.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn external_branch_rewind_fails_closed_and_joins_instead_of_retrying() {
        let (session, observer, failures) = fixture().await;
        let observation = begin(&observer, "tp-private-rewind").await;
        let assistant = observer
            .core
            .transaction
            .lock()
            .await
            .children
            .values()
            .next()
            .unwrap()
            .begin
            .origin
            .assistant_entry_id
            .clone();
        session.move_lane("main", Some(&assistant)).await.unwrap();
        let result = Arc::new(signed("cannot write after rebase"));
        let weak = Arc::downgrade(&result);
        observation.update(result).unwrap();
        let error = timeout(
            Duration::from_secs(3),
            observation.close(Some("tp-private-must-not-leak")),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(observation.close(None).await.unwrap_err(), error);
        assert!(weak.upgrade().is_none());
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        let (_, phases) = records(&session).await;
        assert!(!phases.iter().any(|p| p.phase == PhaseKind::Final));
    }

    #[tokio::test]
    async fn begin_failure_never_spawns_or_retains_a_worker() {
        let (session, observer, failures) = fixture().await;
        let revision = session.storage().history_revision().await.unwrap();
        let weak_core = Arc::downgrade(&observer.core);
        let invalid_args = json!("x".repeat(MAX_BODY_BYTES));
        assert!(observer
            .begin(
                child("tp-private-setup", "root-call-native"),
                &invalid_args,
                CancellationToken::new()
            )
            .await
            .is_err());
        assert_eq!(
            session.storage().history_revision().await.unwrap(),
            revision
        );
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&observer.core), 1);
        drop(observer);
        assert!(weak_core.upgrade().is_none());
    }

    #[tokio::test]
    async fn pinned_session_survives_facade_binding_switch() {
        let (session, observer, failures) = fixture().await;
        let original = observer.core.session.clone();
        let observation = begin(&observer, "tp-private-pin").await;
        let replacement = memory("replacement");
        session.set_storage(replacement.storage());
        observation
            .update(Arc::new(signed("old session only")))
            .unwrap();
        observation
            .finish(&signed("final"), false, false)
            .await
            .unwrap();
        observation.close(None).await.unwrap();
        assert!(records(&session).await.0.is_empty());
        let (begins, phases) = records(&original).await;
        assert_eq!(begins.len(), 1);
        assert_eq!(phases.last().unwrap().phase, PhaseKind::Closed);
        assert_eq!(failures.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancelled_prepared_releases_barrier_and_sticky_failure_still_joins() {
        let (session, observer, failures) = fixture().await;
        let observation = begin(&observer, "tp-private-cancel-prepared").await;
        let arguments = json!({"prepared":true});
        let mut pending = observation.prepared(&arguments);
        assert!(futures::poll!(pending.as_mut()).is_pending());
        drop(pending);
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        assert!(timeout(Duration::from_secs(3), observation.close(None))
            .await
            .unwrap()
            .is_err());
        let (_, phases) = records(&session).await;
        assert_eq!(phases.last().unwrap().phase, PhaseKind::Closed);
        assert_eq!(phases.last().unwrap().interrupted, Some(true));
        assert!(!phases.iter().any(|p| p.phase == PhaseKind::Final));
    }

    #[tokio::test]
    async fn cancelled_close_keeps_join_owned_and_second_close_drains() {
        let (_session, observer, failures) = fixture().await;
        let observation = begin(&observer, "tp-private-cancel-close").await;
        let tx = observer.core.transaction.lock().await;
        let result = Arc::new(signed("accepted before close"));
        let weak = Arc::downgrade(&result);
        observation.update(result).unwrap();
        let mut pending = observation.close(None);
        assert!(futures::poll!(pending.as_mut()).is_pending());
        drop(pending);
        drop(tx);
        let error = timeout(Duration::from_secs(3), observation.close(None))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(observation.close(None).await.unwrap_err(), error);
        assert!(weak.upgrade().is_none());
        assert_eq!(failures.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn final_body_quota_failure_never_fabricates_final_and_close_records_interrupted() {
        let (session, observer, failures) = fixture().await;
        let observation = begin(&observer, "tp-private-final-quota").await;
        let oversized = AgentToolResult::text("x".repeat(MAX_BODY_BYTES));
        assert!(observation.finish(&oversized, false, false).await.is_err());
        assert!(observation.close(None).await.is_err());
        let (_, phases) = records(&session).await;
        assert!(!phases.iter().any(|p| p.phase == PhaseKind::Final));
        assert_eq!(phases.last().unwrap().interrupted, Some(true));
        assert_eq!(failures.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dropping_unclosed_driver_aborts_worker_without_session_cycle() {
        let (_session, observer, _) = fixture().await;
        let observation = begin(&observer, "tp-private-drop").await;
        let weak_core = Arc::downgrade(&observer.core);
        drop(observation);
        drop(observer);
        timeout(Duration::from_secs(3), async {
            while weak_core.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
