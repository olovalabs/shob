use std::{future::Future, pin::Pin, sync::Arc};

use agent::{error::AgentError, types::AgentToolResult};
use async_trait::async_trait;
use harness::{
    agent_harness::{AgentHarness, AgentLane},
    types::HarnessTool,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tools::tools::external::{
    create_external_tool, validate_external_tool_definitions, ExternalToolBackend,
    ExternalToolDefinition,
};

use crate::TextContentOrImage;

type HostToolFuture = Pin<Box<dyn Future<Output = Result<Vec<TextContentOrImage>, String>> + Send>>;

/// A model-facing tool executed by the embedding host. Cancellation drops the call future.
#[derive(Clone)]
pub struct HostTool {
    definition: ExternalToolDefinition,
    call: Arc<dyn Fn(Value) -> HostToolFuture + Send + Sync>,
}

impl HostTool {
    pub fn new(
        definition: ExternalToolDefinition,
        call: impl Fn(Value) -> HostToolFuture + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            call: Arc::new(call),
        }
    }
}

struct HostBackend(Arc<dyn Fn(Value) -> HostToolFuture + Send + Sync>);

#[async_trait]
impl ExternalToolBackend for HostBackend {
    async fn execute(
        &self,
        _name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<AgentToolResult, AgentError> {
        if cancel.is_cancelled() {
            return Err(AgentError::Abort);
        }
        if agent::tool_execution_scope::is_child_execution() {
            return Err(AgentError::Tool(
                "Host tools are not available to child agents.".into(),
            ));
        }
        let content = (self.0)(arguments).await.map_err(AgentError::Tool)?;
        Ok(AgentToolResult {
            content,
            ..Default::default()
        })
    }
}

pub(crate) async fn install(harness: &AgentHarness, tools: Vec<HostTool>) -> Result<(), String> {
    let mut registry = harness
        .get_tools()
        .await
        .map_err(|error| error.to_string())?;
    let reserved = registry
        .iter()
        .map(|tool| tool.tool.schema().name.clone())
        .collect::<Vec<_>>();
    let (definitions, callbacks): (Vec<_>, Vec<_>) = tools
        .into_iter()
        .map(|tool| (tool.definition, tool.call))
        .unzip();
    validate_external_tool_definitions(&definitions, &reserved)
        .map_err(|error| error.to_string())?;
    let mut active = harness
        .get_active_tools()
        .await
        .map_err(|error| error.to_string())?;
    active.extend(definitions.iter().map(|definition| definition.name.clone()));
    registry.extend(
        definitions
            .into_iter()
            .zip(callbacks)
            .map(|(definition, call)| {
                HarnessTool::new(create_external_tool(
                    definition,
                    Arc::new(HostBackend(call)),
                ))
            }),
    );
    harness
        .set_tools(registry, Some(active))
        .await
        .map_err(|error| error.to_string())
}
