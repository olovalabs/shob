//! Compose the installed native journal with an explicit host observation sink.
//! Neither replaces the other. Any sink failure fails the run; both close paths
//! are awaited even when one reports an error. The journal owns the public ID.
use std::sync::Arc;

use futures::{future::BoxFuture, FutureExt};
use rpi_agent::{
    tool_program_observation::{ToolProgramChild, ToolProgramObservation, ToolProgramObserver},
    AgentToolResult,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::tool_program_journal::Fail;

type Observation = Arc<dyn ToolProgramObservation>;
type Observer = Arc<dyn ToolProgramObserver>;

pub(crate) struct CombinedObserver {
    pub journal: Observer,
    pub extra: Observer,
    pub fail: Fail,
}

async fn caught<T>(
    future: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    std::panic::AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err("host child observation panicked".into()))
}
impl ToolProgramObserver for CombinedObserver {
    fn begin<'a>(
        &'a self,
        child: ToolProgramChild,
        arguments: &'a Value,
        signal: CancellationToken,
    ) -> BoxFuture<'a, Result<Observation, String>> {
        Box::pin(async move {
            let journal = self
                .journal
                .begin(child.clone(), arguments, signal.clone())
                .await?;
            // Catch construction as well as polling: the journal already owns a
            // worker and must be closed if the second begin fails or panics.
            let extra = caught(async { self.extra.begin(child, arguments, signal).await }).await;
            match extra {
                Ok(extra) => Ok(Arc::new(CombinedObservation {
                    journal,
                    extra,
                    fail: self.fail.clone(),
                }) as Observation),
                Err(error) => {
                    (self.fail)(error.clone());
                    let _ = caught(async { journal.close(Some("host begin failed")).await }).await;
                    Err(error)
                }
            }
        })
    }
}
struct CombinedObservation {
    journal: Observation,
    extra: Observation,
    fail: Fail,
}
impl CombinedObservation {
    fn checked(&self, result: Result<(), String>) -> Result<(), String> {
        if let Err(error) = &result {
            (self.fail)(error.clone());
        }
        result
    }
}
impl ToolProgramObservation for CombinedObservation {
    fn id(&self) -> &str {
        self.journal.id()
    }
    fn update(&self, partial: Arc<AgentToolResult>) -> Result<(), String> {
        self.checked(self.journal.update(partial.clone()))?;
        self.checked(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.extra.update(partial)))
                .unwrap_or_else(|_| Err("host child observation update panicked".into())),
        )
    }
    fn prepared<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.checked(self.journal.prepared(arguments).await)?;
            self.checked(
                caught(async { self.extra.prepared(arguments).await })
                    .await
                    .map_err(|error| format!("host child observation prepared: {error}")),
            )
        })
    }
    fn finish<'a>(
        &'a self,
        result: &'a AgentToolResult,
        is_error: bool,
        cancelled: bool,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.checked(self.journal.finish(result, is_error, cancelled).await)?;
            self.checked(
                caught(async { self.extra.finish(result, is_error, cancelled).await }).await,
            )
        })
    }
    fn close<'a>(&'a self, reason: Option<&'a str>) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let journal = caught(async { self.journal.close(reason).await }).await;
            let extra = caught(async { self.extra.close(reason).await }).await;
            self.checked(journal.and(extra))
        })
    }
}
