//! Protocol-neutral, model-facing tools supplied by an external backend.
//!
//! Definitions and arguments are untrusted input: validation does not grant
//! filesystem access, establish caller provenance, or install additional tools.
//! The backend remains responsible for authorization and resource cleanup.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use jsonschema::JSONSchema;
use rpi_agent::{
    agent_tool::AgentTool,
    error::AgentError,
    types::{AgentToolResult, ToolResultPartial},
};
use rpi_ai::types::{Schema, Tool};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// A caller-supplied tool definition. Limits are measured in UTF-8 bytes.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// The backend owns any resources it creates and must honor cancellation.
/// Its execution future may be dropped on cancellation; detached resources must
/// still be cleaned up. Arguments are forwarded unchanged after validation.
#[async_trait]
pub trait ExternalToolBackend: Send + Sync {
    async fn execute(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<AgentToolResult, AgentError>;
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::Validation(message.into())
}

// Only root-local JSON Pointers are supported. Keep the default draft aligned
// with JSONSchema::compile (Draft 7); newer dialects can add reference semantics
// that this guard does not understand. All accepted meta-schemas are built in.
fn check_refs(root: &Value) -> Result<(), AgentError> {
    const MAX_NODES: usize = 4096;
    const MAX_EXPANDED_NODES: usize = 16384;
    const MAX_DEPTH: usize = 64;

    // A node is an actual schema, not an arbitrary JSON object. In particular,
    // const/default/enum/examples and extension annotations are opaque data.
    let mut schemas = vec![root];
    let mut indices = HashMap::from([(root as *const Value, 0usize)]);
    let mut edges: Vec<Vec<usize>> = vec![Vec::new()];
    let mut references = Vec::new();
    let mut cursor = 0;
    while cursor < schemas.len() {
        let schema = schemas[cursor];
        if let Some(object) = schema.as_object() {
            if let Some(dialect) = object.get("$schema") {
                if !dialect.as_str().is_some_and(|s| {
                    matches!(
                        s.strip_suffix('#').unwrap_or(s),
                        "http://json-schema.org/draft-04/schema"
                            | "http://json-schema.org/draft-06/schema"
                            | "http://json-schema.org/draft-07/schema"
                    )
                }) {
                    return Err(invalid(
                        "external tool schema uses an unsupported $schema dialect",
                    ));
                }
            }
            // Even fragment IDs introduce aliases / resolution scopes. Exclude
            // them so every reference has exactly the root-pointer meaning.
            for key in ["$id", "id"] {
                if object.contains_key(key) {
                    return Err(invalid(
                        "external tool schemas cannot set base URI or anchor IDs",
                    ));
                }
            }
            for key in [
                "$recursiveRef",
                "$dynamicRef",
                "$recursiveAnchor",
                "$dynamicAnchor",
                "$anchor",
            ] {
                if object.contains_key(key) {
                    return Err(invalid(
                        "external tool schemas do not support anchor or dynamic references",
                    ));
                }
            }
            if let Some(reference) = object.get("$ref") {
                let reference = reference
                    .as_str()
                    .ok_or_else(|| invalid("external tool $ref must be a string"))?;
                let pointer = reference.strip_prefix('#').ok_or_else(|| {
                    invalid("external tool schemas permit only root-local fragment $ref values")
                })?;
                if !pointer.is_empty() && !pointer.starts_with('/') {
                    return Err(invalid(
                        "external tool $ref fragments must be JSON Pointers, not anchors",
                    ));
                }
                // URL fragment percent decoding is deliberately unsupported;
                // accepting it without matching the resolver would hide edges.
                if pointer.contains('%') || pointer.chars().any(|c| c.is_control() || c == ' ') {
                    return Err(invalid(
                        "external tool $ref requires an unencoded JSON Pointer",
                    ));
                }
                for token in pointer.split('/').skip(1) {
                    let mut chars = token.chars();
                    while let Some(c) = chars.next() {
                        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
                            return Err(invalid(
                                "external tool $ref has an invalid JSON Pointer escape",
                            ));
                        }
                    }
                }
                let target = root
                    .pointer(pointer)
                    .ok_or_else(|| invalid("external tool $ref target does not exist"))?;
                references.push((cursor, target as *const Value));
            }

            let mut children = Vec::new();
            for (key, value) in object {
                match key.as_str() {
                    "properties" | "patternProperties" | "definitions" | "$defs"
                    | "dependentSchemas" | "dependencies" => {
                        if let Some(map) = value.as_object() {
                            children
                                .extend(map.values().filter(|v| v.is_object() || v.is_boolean()));
                        }
                    }
                    "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                        if let Some(array) = value.as_array() {
                            children.extend(array);
                        }
                    }
                    "items" => {
                        if let Some(array) = value.as_array() {
                            children.extend(array);
                        } else {
                            children.push(value);
                        }
                    }
                    "additionalProperties"
                    | "additionalItems"
                    | "propertyNames"
                    | "contains"
                    | "not"
                    | "if"
                    | "then"
                    | "else"
                    | "unevaluatedProperties"
                    | "unevaluatedItems"
                    | "contentSchema" => {
                        children.push(value);
                    }
                    _ => {}
                }
            }
            for child in children {
                if !child.is_object() && !child.is_boolean() {
                    return Err(invalid(
                        "external tool subschemas must be objects or booleans",
                    ));
                }
                let next = schemas.len();
                let index = *indices.entry(child as *const Value).or_insert(next);
                if index == next {
                    if next >= MAX_NODES {
                        return Err(invalid("external tool schema graph exceeds node budget"));
                    }
                    schemas.push(child);
                    edges.push(Vec::new());
                }
                edges[cursor].push(index);
            }
        }
        cursor += 1;
    }
    let has_references = !references.is_empty();
    for (source, target) in references {
        let index = indices.get(&target).ok_or_else(|| {
            invalid("external tool $ref must target a recognized schema position")
        })?;
        edges[source].push(*index);
    }

    // jsonschema 0.18 also indexes IDs inside annotations (except const/enum).
    // An annotation ID must not shadow one of our root-pointer targets. This is
    // a resolver ambiguity check, not a scan of literal $ref or base-URI data.
    // Without schema IDs, only an ID containing a fragment can shadow a
    // nonempty root pointer (including absolute json-schema:/// aliases).
    // Ordinary literal IDs and literal $ref values remain unrestricted.
    if has_references {
        let mut values = vec![root];
        while let Some(value) = values.pop() {
            match value {
                Value::Object(object) => {
                    if !indices.contains_key(&(value as *const Value))
                        && ["id", "$id"].iter().any(|key| {
                            object
                                .get(*key)
                                .and_then(Value::as_str)
                                .is_some_and(|id| id.contains('#'))
                        })
                    {
                        return Err(invalid(
                            "external tool local references cannot use annotation ID aliases",
                        ));
                    }
                    values.extend(
                        object
                            .iter()
                            .filter(|(key, _)| *key != "const" && *key != "enum")
                            .map(|(_, value)| value),
                    );
                }
                Value::Array(array) => values.extend(array),
                _ => {}
            }
        }
    }

    // Iterative tri-color DFS includes containment AND reference edges: this
    // rejects consuming recursive schemas too, including unused definitions.
    // Bound expansion as well as depth: a small acyclic ref DAG can otherwise
    // expand exponentially in the validator. This is not hard preemption of
    // arbitrary validator work (e.g. regular-expression execution).
    let mut colors = vec![0u8; schemas.len()];
    let mut costs = vec![1usize; schemas.len()];
    let mut depths = vec![1usize; schemas.len()];
    for start in 0..schemas.len() {
        if colors[start] != 0 {
            continue;
        }
        colors[start] = 1;
        let mut stack = vec![(start, 0usize)];
        while let Some((node, next)) = stack.last_mut() {
            if *next == edges[*node].len() {
                colors[*node] = 2;
                stack.pop();
                continue;
            }
            let child = edges[*node][*next];
            if colors[child] == 1 {
                return Err(invalid(
                    "external tool schema reference cycles are not supported",
                ));
            }
            if colors[child] == 0 {
                colors[child] = 1;
                stack.push((child, 0));
                if stack.len() > MAX_DEPTH {
                    return Err(invalid("external tool schema exceeds depth budget"));
                }
                continue;
            }
            costs[*node] = costs[*node].saturating_add(costs[child]);
            depths[*node] = depths[*node].max(depths[child] + 1);
            if costs[*node] > MAX_EXPANDED_NODES || depths[*node] > MAX_DEPTH {
                return Err(invalid(
                    "external tool schema exceeds expansion or depth budget",
                ));
            }
            *next += 1;
        }
    }
    Ok(())
}

fn compile(definition: &ExternalToolDefinition) -> Result<JSONSchema, AgentError> {
    let name = &definition.name;
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(invalid(
            "external tool name must contain 1..64 ASCII letters, digits, '_' or '-'",
        ));
    }
    if definition.description.len() > 8192 {
        return Err(invalid("external tool description exceeds 8192 bytes"));
    }
    if !definition.input_schema.is_object()
        || definition.input_schema.get("type").and_then(Value::as_str) != Some("object")
    {
        return Err(invalid(
            "external tool inputSchema must be an object with type 'object'",
        ));
    }
    check_refs(&definition.input_schema)?;
    JSONSchema::compile(&definition.input_schema)
        .map_err(|e| invalid(format!("invalid external tool schema: {e}")))
}

/// Validate a complete registration batch before installing any tools.
/// Reserved names are compared exactly, as are duplicate names.
pub fn validate_external_tool_definitions(
    definitions: &[ExternalToolDefinition],
    reserved: &[String],
) -> Result<(), AgentError> {
    if definitions.len() > 128 {
        return Err(invalid("at most 128 external tools are allowed"));
    }
    let bytes = serde_json::to_vec(definitions).map_err(|e| invalid(e.to_string()))?;
    if bytes.len() > 256 * 1024 {
        return Err(invalid("external tool definitions exceed 256 KiB"));
    }
    let mut names = HashSet::new();
    for definition in definitions {
        if reserved.contains(&definition.name) {
            return Err(invalid(format!(
                "reserved external tool name: {}",
                definition.name
            )));
        }
        if !names.insert(&definition.name) {
            return Err(invalid(format!(
                "duplicate external tool name: {}",
                definition.name
            )));
        }
        compile(definition)?;
    }
    Ok(())
}

struct ExternalTool {
    schema: Tool,
    validator: Result<JSONSchema, AgentError>,
    backend: Arc<dyn ExternalToolBackend>,
}

/// Create a tool with the supplied schema, without coercion or fallback.
/// Validate the entire batch first with [`validate_external_tool_definitions`].
/// An invalid individual definition fails closed on execution, never calling the backend.
pub fn create_external_tool(
    definition: ExternalToolDefinition,
    backend: Arc<dyn ExternalToolBackend>,
) -> Arc<dyn AgentTool> {
    let validator = validate_external_tool_definitions(std::slice::from_ref(&definition), &[])
        .and_then(|()| compile(&definition));
    Arc::new(ExternalTool {
        schema: Tool {
            name: definition.name,
            description: definition.description,
            parameters: Schema::new(definition.input_schema),
            constrained_sampling: None,
        },
        validator,
        backend,
    })
}

#[async_trait]
impl AgentTool for ExternalTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        &self.schema.name
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        let validator = self.validator.as_ref().map_err(Clone::clone)?;
        if let Err(mut errors) = validator.validate(&params) {
            return Err(invalid(
                errors
                    .next()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "invalid external tool arguments".into()),
            ));
        }
        tokio::select! {
            biased;
            _ = signal.cancelled() => Err(AgentError::Abort),
            result = self.backend.execute(&self.schema.name, params, signal.clone()) => {
                if signal.is_cancelled() { Err(AgentError::Abort) } else { result }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::types::TextContentOrImage;
    use rpi_ai::types::{ImageContent, ImageContentType};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn definition() -> ExternalToolDefinition {
        ExternalToolDefinition {
            name: "external-test_1".into(),
            description: "Real external tool".into(),
            input_schema: json!({"type":"object", "properties":{"value":{"type":"integer"}}, "required":["value"], "additionalProperties":false}),
        }
    }

    #[derive(Default)]
    struct Backend {
        calls: AtomicUsize,
        wait: bool,
        started: tokio::sync::Notify,
    }

    fn response() -> AgentToolResult {
        AgentToolResult {
            content: vec![
                TextContentOrImage::text("first"),
                TextContentOrImage::Image(ImageContent {
                    kind: ImageContentType,
                    data: "aW1hZ2U=".into(),
                    mime_type: "image/png".into(),
                }),
                TextContentOrImage::text("last"),
            ],
            details: json!({"nested":{"unchanged":true}}),
            terminate: true,
            ..Default::default()
        }
    }

    #[async_trait]
    impl ExternalToolBackend for Backend {
        async fn execute(
            &self,
            name: &str,
            arguments: Value,
            cancel: CancellationToken,
        ) -> Result<AgentToolResult, AgentError> {
            assert_eq!(name, "external-test_1");
            assert_eq!(arguments, json!({"value":7}));
            assert!(!cancel.is_cancelled());
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if self.wait {
                // Deliberately ignore cancellation: the factory must stop waiting
                // even when a backend fails to cooperate.
                return std::future::pending().await;
            }
            Ok(response())
        }
    }

    #[tokio::test]
    async fn factory_preserves_schema_and_complete_result() {
        let backend = Arc::new(Backend::default());
        let def = definition();
        let expected_schema = Schema::new(def.input_schema.clone());
        let expected_description = def.description.clone();
        let tool = create_external_tool(def, backend.clone());
        assert_eq!(tool.schema().parameters, expected_schema);
        assert_eq!(tool.schema().description, expected_description);
        assert!(tool.schema().constrained_sampling.is_none());
        assert_eq!(tool.label(), "external-test_1");
        let result = tool
            .execute(
                "id",
                json!({"value":7}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        assert_eq!(result.content, response().content);
        assert_eq!(result.details, response().details);
        assert!(result.terminate);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn definitions_are_strict_and_bounded() {
        let def = definition();
        let encoded = serde_json::to_value(&def).unwrap();
        assert!(encoded.get("inputSchema").is_some());
        let mut extra = encoded;
        extra["unexpected"] = json!(true);
        assert!(serde_json::from_value::<ExternalToolDefinition>(extra).is_err());
        assert!(validate_external_tool_definitions(std::slice::from_ref(&def), &[]).is_ok());
        assert!(validate_external_tool_definitions(&[def.clone(), def.clone()], &[]).is_err());
        assert!(validate_external_tool_definitions(
            std::slice::from_ref(&def),
            std::slice::from_ref(&def.name)
        )
        .is_err());
        for name in ["", "has space", "unicodeé", &"a".repeat(65)] {
            let mut bad = def.clone();
            bad.name = name.into();
            assert!(validate_external_tool_definitions(&[bad], &[]).is_err());
        }
        let mut boundary = def.clone();
        boundary.name = "a".repeat(64);
        boundary.description = "x".repeat(8192);
        assert!(validate_external_tool_definitions(&[boundary.clone()], &[]).is_ok());
        boundary.description.push('x');
        assert!(validate_external_tool_definitions(&[boundary], &[]).is_err());
        let mut batch: Vec<_> = (0..128)
            .map(|i| {
                let mut d = def.clone();
                d.name = format!("tool{i}");
                d
            })
            .collect();
        assert!(validate_external_tool_definitions(&batch, &[]).is_ok());
        batch.push(def.clone());
        assert!(validate_external_tool_definitions(&batch, &[]).is_err());
        batch.pop();
        for d in &mut batch {
            d.description = "x".repeat(8192);
        }
        assert!(validate_external_tool_definitions(&batch, &[]).is_err());
        for schema in [
            json!(true),
            json!({}),
            json!({"type":"array"}),
            json!({"type":"object","properties":{"x":{"type":42}}}),
            json!({"type":"object","$ref":"https://example.com/schema"}),
            json!({"type":"object","definitions":{"unused":{"$ref":"file:///secret"}}}),
            json!({"type":"object","$ref":42}),
            json!({"type":"object","$ref":""}),
            json!({"type":"object","$ref":"//example.com/schema"}),
            json!({"type":"object","$id":"https://example.com/base","$ref":"#"}),
            json!({"type":"object","id":"file:///secret","$ref":"#"}),
            json!({"type":"object","definitions":{"unused":{"$id":"relative/base"}}}),
            json!({"type":"object","allOf":[{"$ref":"relative.json#/x"}]}),
            json!({"type":"object","$id":42}),
        ] {
            let mut bad = def.clone();
            bad.input_schema = schema;
            assert!(validate_external_tool_definitions(&[bad], &[]).is_err());
        }
        let mut local = def;
        local.input_schema = json!({"type":"object","properties":{"value":{"$ref":"#/definitions/n"}},"definitions":{"n":{"type":"integer"}}});
        assert!(validate_external_tool_definitions(&[local], &[]).is_ok());
    }

    #[test]
    fn external_definition_size_boundary_and_schema_map_names() {
        let mut def = definition();
        def.input_schema = json!({
            "type":"object",
            "properties":{
                "id":{"type":"string"},
                "$id":{"type":"string"},
                "$ref":{"$ref":"#/definitions/id"}
            },
            "definitions":{"id":{"type":"integer"}},
            "x-padding":""
        });
        assert!(validate_external_tool_definitions(&[def.clone()], &[]).is_ok());
        let bytes = serde_json::to_vec(std::slice::from_ref(&def))
            .unwrap()
            .len();
        def.input_schema["x-padding"] = json!("x".repeat(256 * 1024 - bytes));
        assert_eq!(
            serde_json::to_vec(std::slice::from_ref(&def))
                .unwrap()
                .len(),
            256 * 1024
        );
        assert!(validate_external_tool_definitions(&[def.clone()], &[]).is_ok());
        def.description.push('x');
        assert!(validate_external_tool_definitions(&[def], &[]).is_err());
    }

    #[test]
    fn external_rejects_reference_cycles_before_compilation() {
        let mut def = definition();
        let mut schemas = vec![
            json!({"type":"object","$ref":"#"}),
            json!({"type":"object","$defs":{"a":{"$ref":"#/$defs/b"},"b":{"$ref":"#/$defs/a"}}}),
            json!({"type":"object","properties":{"child":{"$ref":"#"}}}),
            json!({"type":"object","definitions":{"a/b~c":{"$ref":"#/definitions/a~1b~0c"}}}),
        ];
        for keyword in ["allOf", "anyOf", "oneOf", "items", "prefixItems"] {
            schemas.push(json!({"type":"object",keyword:[{"$ref":"#"}]}));
        }
        for keyword in [
            "not",
            "contains",
            "propertyNames",
            "additionalItems",
            "additionalProperties",
            "items",
            "if",
            "then",
            "else",
            "unevaluatedProperties",
            "unevaluatedItems",
            "contentSchema",
        ] {
            schemas.push(json!({"type":"object",keyword:{"$ref":"#"}}));
        }
        for keyword in [
            "properties",
            "patternProperties",
            "definitions",
            "$defs",
            "dependentSchemas",
            "dependencies",
        ] {
            schemas.push(json!({"type":"object",keyword:{"unused":{"$ref":"#"}}}));
        }
        for schema in schemas {
            def.input_schema = schema.clone();
            let error = validate_external_tool_definitions(&[def.clone()], &[]).unwrap_err();
            assert!(error.to_string().contains("cycles"), "{schema}: {error}");
        }
    }

    #[test]
    fn external_references_fail_closed_for_unsupported_resolution() {
        let mut def = definition();
        for schema in [
            json!({"type":"object","$ref":"#anchor"}),
            json!({"type":"object","$ref":"#/%64efinitions/a","definitions":{"a":{}}}),
            json!({"type":"object","$ref":"#/definitions/a~2b","definitions":{"a~2b":{}}}),
            json!({"type":"object","$ref":"#/missing"}),
            json!({"type":"object","$ref":"#/const","const":{"$ref":"#"}}),
            json!({"type":"object","$ref":"http://127.0.0.1:9/schema"}),
            json!({"type":"object","$schema":"http://127.0.0.1:9/meta"}),
            json!({"type":"object","$schema":"https://json-schema.org/draft/2020-12/schema","$dynamicRef":"#"}),
            json!({"type":"object","$defs":{"a":{"$schema":"https://json-schema.org/draft/2019-09/schema","$recursiveRef":"#"}}}),
            json!({"type":"object","$dynamicRef":"#"}),
            json!({"type":"object","$recursiveRef":"#"}),
            json!({"type":"object","$id":"#anchor"}),
            json!({"type":"object","$ref":"#/definitions/a","definitions":{"a":{}},"default":{"$id":"#/definitions/a","$ref":"#/definitions/a"}}),
            json!({"type":"object","$ref":"#/definitions/a","definitions":{"a":{}},"default":{"$id":"json-schema:///#/definitions/a","$ref":"#/definitions/a"}}),
        ] {
            def.input_schema = schema.clone();
            assert!(
                validate_external_tool_definitions(&[def.clone()], &[]).is_err(),
                "{schema}"
            );
        }
    }

    #[test]
    fn external_rejects_network_references_and_meta_schemas_without_connecting() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/schema", listener.local_addr().unwrap());
        let mut def = definition();
        for schema in [
            json!({"type":"object","$schema":url}),
            json!({"type":"object","$ref":url}),
            json!({"type":"object","definitions":{"unused":{"$ref":url}}}),
        ] {
            def.input_schema = schema;
            assert!(validate_external_tool_definitions(&[def.clone()], &[]).is_err());
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

    #[test]
    fn external_acyclic_escaped_references_and_literals_are_valid() {
        let mut def = definition();
        def.input_schema = json!({
            "type":"object",
            "properties":{"value":{"$ref":"#/$defs/a~1b~0c"}},
            "$defs":{
                "a/b~c":{"$ref":"#/definitions/integer"},
                "literal":{"const":{"id":"ordinary-value","$id":"file:///data","$ref":"#"}}
            },
            "definitions":{"integer":{"type":"integer"}},
            "required":["value"],
            "default":{"id":"ordinary-value","$id":"relative/data","$ref":"file:///data"},
            "examples":[{"id":"ordinary-value","$ref":"#"}],
            "enum":[{"value":7,"$ref":"#"}],
            "dependencies":{"value":["value"]}
        });
        validate_external_tool_definitions(&[def.clone()], &[]).unwrap();
        let validator = compile(&def).unwrap();
        assert!(validator.is_valid(&json!({"value":7,"$ref":"#"})));
        assert!(!validator.is_valid(&json!({"value":"7","$ref":"#"})));
        for dialect in [
            "http://json-schema.org/draft-04/schema#",
            "http://json-schema.org/draft-06/schema#",
            "http://json-schema.org/draft-07/schema#",
        ] {
            def.input_schema = json!({"type":"object","$schema":dialect,"properties":{"value":{"$ref":"#/definitions/n"}},"definitions":{"n":{"type":"integer"}}});
            validate_external_tool_definitions(&[def.clone()], &[]).unwrap();
            assert!(compile(&def).unwrap().is_valid(&json!({"value":7})));
        }
        for literal_keyword in ["const", "default", "enum", "examples"] {
            let literal = json!({"id":"ordinary-value","$id":"relative/data","$ref":"https://example.com/data"});
            let literal = if matches!(literal_keyword, "enum" | "examples") {
                json!([literal])
            } else {
                literal
            };
            def.input_schema = json!({"type":"object",literal_keyword:literal});
            validate_external_tool_definitions(&[def.clone()], &[]).unwrap();
        }
        // The review's exact compatibility repro: const is data, not a schema.
        def.input_schema = json!({"type":"object","properties":{"x":{"const":{"id":"ordinary-value","$ref":"https://example.com/data"}}}});
        validate_external_tool_definitions(&[def], &[]).unwrap();
    }

    #[test]
    fn external_rejects_explosive_acyclic_reference_dag_and_deep_chain() {
        for (count, branching) in [(16, true), (70, false)] {
            let mut defs = serde_json::Map::new();
            for index in 0..count {
                let next = format!("#/$defs/n{}", index + 1);
                let schema = if index + 1 == count {
                    json!({"type":"object"})
                } else if branching {
                    json!({"allOf":[{"$ref":next},{"$ref":next}]})
                } else {
                    json!({"$ref":next})
                };
                defs.insert(format!("n{index}"), schema);
            }
            let mut def = definition();
            def.input_schema = json!({"type":"object","$ref":"#/$defs/n0","$defs":defs});
            let error = validate_external_tool_definitions(&[def], &[]).unwrap_err();
            assert!(error.to_string().contains("budget"), "{error}");
        }
    }

    #[tokio::test]
    async fn external_cyclic_factory_schema_fails_without_backend() {
        let backend = Arc::new(Backend::default());
        let mut def = definition();
        def.input_schema = json!({"type":"object","$ref":"#"});
        let tool = create_external_tool(def, backend.clone());
        assert!(matches!(
            tool.execute("id", json!({}), CancellationToken::new(), Arc::new(|_| {}))
                .await,
            Err(AgentError::Validation(_))
        ));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn external_fragment_reference_validates_arguments_before_backend() {
        let backend = Arc::new(Backend::default());
        let mut def = definition();
        def.input_schema = json!({
            "type":"object",
            "properties":{"value":{"$ref":"#/definitions/n"}},
            "definitions":{"n":{"type":"integer"}},
            "required":["value"],
            "additionalProperties":false
        });
        let tool = create_external_tool(def, backend.clone());
        assert!(matches!(
            tool.execute(
                "id",
                json!({"value":"7"}),
                CancellationToken::new(),
                Arc::new(|_| {})
            )
            .await,
            Err(AgentError::Validation(_))
        ));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        tool.execute(
            "id",
            json!({"value":7}),
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_arguments_and_cancel_never_call_backend() {
        let backend = Arc::new(Backend::default());
        let tool = create_external_tool(definition(), backend.clone());
        for args in [
            json!({}),
            json!({"value":"7"}),
            json!({"value":7,"extra":true}),
            json!([]),
        ] {
            assert!(matches!(
                tool.execute("id", args, CancellationToken::new(), Arc::new(|_| {}))
                    .await,
                Err(AgentError::Validation(_))
            ));
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            tool.execute("id", json!({"value":7}), cancel, Arc::new(|_| {}))
                .await,
            Err(AgentError::Abort)
        ));
        let mut bad = definition();
        bad.input_schema = json!({});
        assert!(matches!(
            create_external_tool(bad, backend.clone())
                .execute("id", json!({}), CancellationToken::new(), Arc::new(|_| {}))
                .await,
            Err(AgentError::Validation(_))
        ));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn in_flight_cancellation_returns_abort() {
        let backend = Arc::new(Backend {
            wait: true,
            ..Default::default()
        });
        let tool = create_external_tool(definition(), backend.clone());
        let cancel = CancellationToken::new();
        let run = tool.execute("id", json!({"value":7}), cancel.clone(), Arc::new(|_| {}));
        let cancellation = async {
            backend.started.notified().await;
            cancel.cancel();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(run, cancellation)
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(AgentError::Abort)));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }
}
