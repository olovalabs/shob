//! Optional host-owned observation of genuine native ToolProgram children.
//!
//! This is not a transcript or an event adapter. The observer owns its writer
//! lifecycle: `close` must be idempotent and join/drain all accepted writes.
//! Implementations must not panic. Requested arguments, partials, and finalized
//! results are native values, not broker envelopes or guest-provided identities.
use crate::AgentToolResult;
use futures::future::BoxFuture;
use serde_json::Value;
use std::{future::Future, sync::Arc};
use tokio_util::sync::CancellationToken;

/// Native invocation identity. `request_id` remains the private `tp-*` call ID;
/// an observation's public entry ID is separate and must not replace it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolProgramChild {
    pub root_id: String,
    pub root_name: String,
    pub parent_id: String,
    pub request_id: String,
    pub name: String,
    pub ordinal: usize,
}

/// The creator owns all resources until `begin` returns `Ok`. On an error it
/// must retire and join partially created writers itself; the driver cannot
/// close a handle it never received. Construction must be panic-safe as well.
/// After success, the driver owns calling `close`, including on cancellation.
/// This seam does not itself install a durable store or advertise RPC support.
pub trait ToolProgramObserver: Send + Sync {
    fn begin<'a>(
        &'a self,
        child: ToolProgramChild,
        arguments: &'a Value,
        signal: CancellationToken,
    ) -> BoxFuture<'a, Result<Arc<dyn ToolProgramObservation>, String>>;
}

pub trait ToolProgramObservation: Send + Sync {
    fn id(&self) -> &str;
    /// Nonblocking acceptance of the actual partial, without replay/truncation.
    /// Queue/byte overflow must return an error, never silently drop a payload.
    /// The driver retires admitted callbacks before `finish` or `close` starts.
    fn update(&self, partial: Arc<AgentToolResult>) -> Result<(), String>;
    /// The native harness calls this at its execution barrier, with prepared args.
    fn prepared<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<(), String>>;
    /// The known post-hook native result, even if transport encoding later fails.
    fn finish<'a>(
        &'a self,
        result: &'a AgentToolResult,
        is_error: bool,
        cancelled: bool,
    ) -> BoxFuture<'a, Result<(), String>>;
    /// Explicitly retire the writer and drain/join its work before returning,
    /// even when reporting an error. Catching a panic cannot guarantee cleanup;
    /// implementations must keep worker ownership panic-safe through shutdown.
    fn close<'a>(&'a self, reason: Option<&'a str>) -> BoxFuture<'a, Result<(), String>>;
}

tokio::task_local! { static CURRENT: Arc<dyn ToolProgramObservation>; }

pub fn current_observation() -> Option<Arc<dyn ToolProgramObservation>> {
    CURRENT.try_with(Clone::clone).ok()
}

/// Install only a native-owned observation around the child execution future.
/// This does not grant guest authority to select an observer or entry identity.
pub async fn scope<F: Future>(
    observation: Arc<dyn ToolProgramObservation>,
    future: F,
) -> F::Output {
    CURRENT.scope(observation, future).await
}
