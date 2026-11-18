use std::io::{self, Write};
use std::sync::Arc;

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use ai::types::Tool;
use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const MAX_CALLS: usize = 4;
const MAX_ITEM_BYTES: usize = 128 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchInput {
    calls: Vec<BatchCall>,
}

#[derive(Deserialize)]
struct BatchCall {
    tool: ReadOnlyTool,
    #[serde(flatten)]
    arguments: serde_json::Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadOnlyTool {
    Read,
    Grep,
    Find,
    Ls,
    Output,
}

impl ReadOnlyTool {
    fn name(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Grep => "grep",
            Self::Find => "find",
            Self::Ls => "ls",
            Self::Output => "output",
        }
    }
}

struct Entry {
    tool: Arc<dyn AgentTool>,
    validator: Result<jsonschema::JSONSchema, String>,
}

#[derive(Serialize)]
struct BatchItem<'a> {
    index: usize,
    tool: &'a str,
    is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<AgentToolResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

struct BatchTool {
    schema: Tool,
    tools: Vec<Entry>,
}

/// Only the caller's enabled read-only tools are eligible; never shell or mutations.
pub fn create_batch_tool(tools: Vec<Arc<dyn AgentTool>>) -> Arc<dyn AgentTool> {
    let tools: Vec<Entry> = tools
        .into_iter()
        .filter(|tool| {
            matches!(
                tool.schema().name.as_str(),
                "read" | "grep" | "find" | "ls" | "output"
            ) && tool.execution_mode() == ToolExecutionMode::Parallel
        })
        .map(|tool| Entry {
            validator: jsonschema::JSONSchema::compile(&argument_schema(&*tool))
                .map_err(|error| error.to_string()),
            tool,
        })
        .collect();
    let parameters = batch_schema(&tools);
    Arc::new(BatchTool {
        schema: Tool {
            name: "batch".into(),
            description: "Run 1–4 independent read-only calls concurrently in one round trip. Each call uses the standalone tool's fields directly, plus tool: for example {\"calls\":[{\"tool\":\"read\",\"path\":\"package.json\"},{\"tool\":\"ls\",\"path\":\"app\"}]}. No arguments wrapper. Only enabled read, grep, find, ls and output tools are allowed. Results stay in input order; each failure is explicit. No shell, mutations or nested batches. Each result is capped at 128 KiB; use the standalone tool for larger results.".into(),
            parameters: ai::types::Schema::new(parameters),
            constrained_sampling: None,
        },
        tools,
    })
}

fn argument_schema(tool: &dyn AgentTool) -> Value {
    let mut schema = tool.schema().parameters.0.clone();
    schema["additionalProperties"] = false.into();
    schema
}

fn batch_schema(tools: &[Entry]) -> Value {
    let mut definitions = serde_json::Map::new();
    let mut variants = Vec::with_capacity(tools.len());
    for entry in tools {
        let name = &entry.tool.schema().name;
        let pointer = format!("#/definitions/{name}");
        let mut schema = argument_schema(&*entry.tool);
        relocate_refs(&mut schema, &pointer);
        schema["properties"]["tool"] = json!({"type": "string", "const": name});
        let required = schema
            .as_object_mut()
            .unwrap()
            .entry("required")
            .or_insert_with(|| json!([]));
        required.as_array_mut().unwrap().push("tool".into());
        definitions.insert(name.clone(), schema);
        variants.push(json!({"$ref": pointer}));
    }
    let items = if variants.is_empty() {
        Value::Bool(false)
    } else {
        json!({"oneOf": variants})
    };
    json!({
        "type": "object",
        "properties": {
            "calls": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_CALLS,
                "items": items
            }
        },
        "required": ["calls"],
        "additionalProperties": false,
        "definitions": definitions
    })
}

fn relocate_refs(schema: &mut Value, pointer: &str) {
    match schema {
        Value::Object(object) => {
            if let Some(Value::String(reference)) = object.get_mut("$ref") {
                if let Some(fragment) = reference.strip_prefix('#') {
                    *reference = format!("{pointer}{fragment}");
                }
            }
            for value in object.values_mut() {
                relocate_refs(value, pointer);
            }
        }
        Value::Array(values) => {
            for value in values {
                relocate_refs(value, pointer);
            }
        }
        _ => {}
    }
}

struct LimitedWriter(Vec<u8>);

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_ITEM_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other(
                "batch result exceeds 128 KiB; use the standalone tool",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl AgentTool for BatchTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "batch"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: BatchInput = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(format!("batch input invalid: {error}")))?;
        if input.calls.is_empty() || input.calls.len() > MAX_CALLS {
            return Err(AgentError::Validation("batch requires 1–4 calls".into()));
        }
        let mut calls = Vec::with_capacity(input.calls.len());
        // Validate the entire batch before any operation starts.
        for (index, call) in input.calls.into_iter().enumerate() {
            let name = call.tool.name();
            let entry = self
                .tools
                .iter()
                .find(|entry| entry.tool.schema().name == name)
                .ok_or_else(|| {
                    AgentError::Validation(format!("batch tool {name} is not enabled"))
                })?;
            let arguments = entry
                .tool
                .prepare_arguments(Value::Object(call.arguments))?;
            let validator = entry.validator.as_ref().map_err(|error| {
                AgentError::Validation(format!("batch schema for {name} is invalid: {error}"))
            })?;
            if let Err(errors) = validator.validate(&arguments) {
                let error = errors
                    .take(1)
                    .map(|error| error.to_string())
                    .collect::<String>();
                return Err(AgentError::Validation(format!(
                    "batch call {index} ({name}): {error}"
                )));
            }
            calls.push((entry, arguments, index));
        }
        if signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        let results = join_all(calls.into_iter().map(|(entry, arguments, index)| {
            let signal = signal.clone();
            async move {
                let name = &entry.tool.schema().name;
                let id = format!("{tool_call_id}:{index}");
                let result = entry.tool.execute(&id, arguments, signal, Arc::new(|_| {})).await;
                let (result, is_error, error) = match result {
                    Ok(result) => (Some(result), false, None),
                    Err(AgentError::StructuredTool(result)) => (Some(*result), true, None),
                    Err(error) => (None, true, Some(error.to_string())),
                };
                let item = BatchItem { index, tool: name, is_error, result, error };
                let mut writer = LimitedWriter(Vec::new());
                if serde_json::to_writer(&mut writer, &item).is_err() {
                    json!({"index": index, "tool": name, "is_error": true, "error": "Result exceeds 128 KiB; use the standalone tool"}).to_string()
                } else {
                    // serde_json writes valid UTF-8.
                    String::from_utf8(writer.0).expect("JSON is UTF-8")
                }
            }
        }))
        .await;
        if signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(format!("[{}]", results.join(",")))],
            details: json!({"calls": results.len(), "max_concurrency": MAX_CALLS}),
            ..Default::default()
        })
    }
}
