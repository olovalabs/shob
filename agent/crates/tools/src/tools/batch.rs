use std::io::{self, Write};
use std::sync::Arc;

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use ai::types::Tool;
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

pub const MAX_CALLS: usize = 8;
const MAX_CONCURRENCY: usize = 4;
const MAX_ITEM_BYTES: usize = 128 * 1024;
pub const MAX_OUTPUT_BYTES: usize = MAX_CALLS * (MAX_ITEM_BYTES + 1) + 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchInput {
    calls: Vec<BatchCall>,
    codemode: Option<Projection>,
}

/// A small declarative result program, not a host-language interpreter.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Projection {
    contains: Option<String>,
    limit: usize,
}

impl Projection {
    fn apply(&self, result: AgentToolResult) -> AgentToolResult {
        let mut output = String::new();
        let mut count = 0;
        'blocks: for block in result.content {
            let TextContentOrImage::Text(text) = block else {
                continue;
            };
            for line in text.text.lines() {
                if !line.contains(self.contains.as_deref().unwrap_or("")) {
                    continue;
                }
                if count == self.limit || output.len() >= 4096 {
                    output.push_str("… (more matches omitted)");
                    break 'blocks;
                }
                let mut end = line.len().min(4096 - output.len());
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                output.push_str(&line[..end]);
                if end < line.len() {
                    output.push_str("… (truncated)");
                    break 'blocks;
                }
                output.push('\n');
                count += 1;
            }
        }
        if output.is_empty() {
            output.push_str("(no matching text lines)");
        }
        AgentToolResult {
            content: vec![TextContentOrImage::text(output)],
            ..Default::default()
        }
    }
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
            description: format!(
                "Run 1–{MAX_CALLS} independent read-only calls in one round trip, with at most {MAX_CONCURRENCY} running at once. Split larger sets across batches. Each call uses the standalone tool's fields directly, plus tool: for example {{\"calls\":[{{\"tool\":\"read\",\"path\":\"package.json\"}},{{\"tool\":\"ls\",\"path\":\"app\"}}]}}. No arguments wrapper. Only enabled read, grep, find, ls and output tools are allowed. Results stay in input order; each failure is explicit. No shell, mutations or nested batches. For code investigation, add optional top-level codemode: {{\"contains\":\"literal\",\"limit\":12}} to return only matching text lines from each successful call (omit contains for first lines). Each result is capped at 128 KiB; use the standalone tool for larger results."
            ),
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
            "codemode": {
                "type": "object", "additionalProperties": false, "required": ["limit"],
                "properties": {
                    "contains": {"type": "string", "maxLength": 256},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100}
                }
            },
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
            return Err(AgentError::Validation(format!(
                "batch requires 1–{MAX_CALLS} calls"
            )));
        }
        let projection = input.codemode.as_ref();
        if projection.is_some_and(|p| {
            !(1..=100).contains(&p.limit)
                || p.contains.as_ref().is_some_and(|s| s.chars().count() > 256)
        }) {
            return Err(AgentError::Validation("invalid codemode projection".into()));
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
        let child = signal.child_token();
        let _cancel = child.clone().drop_guard();
        let pending = calls
            .into_iter()
            .map(|(entry, arguments, index): (&Entry, Value, usize)| {
                let signal = child.clone();
                async move {
                    let name = entry.tool.schema().name.as_str();
                    let id = format!("{tool_call_id}:{index}");
                    let result = if signal.is_cancelled() {
                        Err(AgentError::Abort)
                    } else {
                        entry.tool.execute(&id, arguments, signal, Arc::new(|_| {})).await
                    };
                    let (result, is_error, error) = match result {
                        Ok(result) => (Some(match projection { Some(p) => p.apply(result), None => result }), false, None),
                        Err(AgentError::StructuredTool(result)) => (Some(*result), true, None),
                        Err(error) => (None, true, Some(error.to_string())),
                    };
                    let item = BatchItem { index, tool: name, is_error, result, error };
                    let mut writer = LimitedWriter(Vec::new());
                    let text = if serde_json::to_writer(&mut writer, &item).is_err() {
                        json!({"index": index, "tool": name, "is_error": true, "error": "Result exceeds 128 KiB; use the standalone tool"}).to_string()
                    } else {
                        String::from_utf8(writer.0).expect("JSON is UTF-8")
                    };
                    (index, text)
                }
            })
            .collect::<Vec<_>>();
        let collect = stream::iter(pending)
            .buffer_unordered(MAX_CONCURRENCY)
            .collect::<Vec<_>>();
        let mut results = if projection.is_some() {
            tokio::select! {
                _ = signal.cancelled() => return Err(AgentError::Abort),
                result = tokio::time::timeout(std::time::Duration::from_secs(30), collect) =>
                    result.map_err(|_| AgentError::Tool("codemode exceeded 30 seconds".into()))?,
            }
        } else {
            tokio::select! {
                _ = signal.cancelled() => return Err(AgentError::Abort),
                result = collect => result,
            }
        };
        if signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        results.sort_unstable_by_key(|(index, _)| *index);
        let count = results.len();
        let size = results.iter().map(|(_, item)| item.len()).sum::<usize>();
        let mut output = String::with_capacity(size + count + 1);
        output.push('[');
        for (index, item) in results {
            if index != 0 {
                output.push(',');
            }
            output.push_str(&item);
        }
        output.push(']');
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(output)],
            details: json!({"calls": count, "max_concurrency": MAX_CONCURRENCY}),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    #[test]
    fn projection_filters_before_limiting() {
        let projection = Projection {
            contains: Some("hit".into()),
            limit: 1,
        };
        let result = projection.apply(AgentToolResult {
            content: vec![TextContentOrImage::text("skip\nhit one\nhit two")],
            ..Default::default()
        });
        let TextContentOrImage::Text(text) = &result.content[0] else {
            panic!()
        };
        assert_eq!(text.text, "hit one\n… (more matches omitted)");
    }
}
