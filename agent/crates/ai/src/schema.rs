//! Mirrors `packages/ai/src/utils/validation.ts` — JSON-Schema validation of tool
//! arguments plus a hand-written `coerce_with_json_schema` layer that tolerates
//! LLM-sloppy JSON (stringified numbers, `"true"`, null on optional fields, …).
//!
//! The TS source uses TypeBox's `Compile`/`Check`/`Value.Convert`; the Rust port
//! replaces TypeBox with the `jsonschema` crate (`JSONSchema::compile` +
//! `is_valid` + `validate`) and a faithful port of the coercion table. The
//! public entry point is [`validate_tool_arguments`], which mirrors TS
//! `validateToolArguments`: `normalize_optional_nulls` → coerce → validate,
//! returning the coerced value or an `AiError::Schema`.

use crate::error::AiError;
use crate::types::{Tool, ToolCall};
use jsonschema::paths::PathChunk;
use jsonschema::JSONSchema;
use serde_json::{Map, Value};
use std::collections::{hash_map::DefaultHasher, HashSet, VecDeque};
use std::hash::Hasher;
use std::sync::{Arc, Mutex};

// ----------------------------------------------------------------------------
// Schema-view helpers — a typed view over the opaque `serde_json::Value` schema
// ----------------------------------------------------------------------------

fn schema_types(schema: &Value) -> Vec<String> {
    let Some(obj) = schema.as_object() else {
        return Vec::new();
    };
    match obj.get("type") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn matches_json_type(value: &Value, ty: &str) -> bool {
    match ty {
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

// ----------------------------------------------------------------------------
// Validator cache — bounded FIFO keyed by a streamed fingerprint and full
// content equality. Hits never allocate serialized strings or trust addresses.
// ----------------------------------------------------------------------------

const VALIDATOR_CACHE_CAPACITY: usize = 64;

struct SchemaFingerprint(DefaultHasher);

impl std::io::Write for SchemaFingerprint {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn schema_fingerprint(schema: &Value) -> u64 {
    let mut fingerprint = SchemaFingerprint(DefaultHasher::new());
    serde_json::to_writer(&mut fingerprint, schema).expect("JSON values are serializable");
    fingerprint.0.finish()
}

#[derive(Default)]
struct ValidatorCache {
    validators: VecDeque<(u64, Value, Arc<JSONSchema>)>,
}

impl ValidatorCache {
    fn get(&self, schema: &Value, fingerprint: u64) -> Option<Arc<JSONSchema>> {
        self.validators
            .iter()
            .find_map(|(key, content, validator)| {
                (*key == fingerprint && content == schema).then(|| Arc::clone(validator))
            })
    }

    fn insert(
        &mut self,
        schema: &Value,
        fingerprint: u64,
        validator: Arc<JSONSchema>,
    ) -> Arc<JSONSchema> {
        if let Some(existing) = self.get(schema, fingerprint) {
            return existing;
        }
        if self.validators.len() == VALIDATOR_CACHE_CAPACITY {
            self.validators.pop_front();
        }
        self.validators
            .push_back((fingerprint, schema.clone(), validator.clone()));
        validator
    }
}

static VALIDATOR_CACHE: Mutex<Option<ValidatorCache>> = Mutex::new(None);

/// Compile-or-fetch a validator for a schema; `None` if compilation fails
/// (mirrors TS `getSubSchemaValidator` returning `undefined` on throw).
fn get_sub_schema_validator(schema: &Value) -> Option<Arc<JSONSchema>> {
    let fingerprint = schema_fingerprint(schema);
    {
        let guard = VALIDATOR_CACHE.lock().unwrap();
        if let Some(validator) = guard
            .as_ref()
            .and_then(|cache| cache.get(schema, fingerprint))
        {
            return Some(validator);
        }
    }
    // Compilation (including reference resolution) must not hold the cache lock.
    // Invalid schemas are not retained, matching TS's try/catch → undefined.
    let validator = Arc::new(JSONSchema::compile(schema).ok()?);
    let mut guard = VALIDATOR_CACHE.lock().unwrap();
    Some(
        guard
            .get_or_insert_with(ValidatorCache::default)
            .insert(schema, fingerprint, validator),
    )
}

// ----------------------------------------------------------------------------
// coercePrimitiveByType — faithful port of the TS table
// ----------------------------------------------------------------------------

/// Coerce a primitive value to match a JSON-Schema primitive type. Mirrors TS
/// `coercePrimitiveByType`. Returns the coerced value (possibly == input when
/// no coercion applies).
fn coerce_primitive_by_type(value: &Value, ty: &str) -> Value {
    match ty {
        "number" => match value {
            Value::Null => Value::from(0),
            Value::String(s) if !s.trim().is_empty() => {
                if let Ok(parsed) = s.trim().parse::<f64>() {
                    if parsed.is_finite() {
                        // Integer-valued numbers become i64 so they compare equal
                        // to integral JSON (and validate under `type:number`).
                        if parsed.fract() == 0.0 {
                            return Value::from(parsed as i64);
                        }
                        return serde_json::Number::from_f64(parsed)
                            .map(Value::Number)
                            .unwrap_or_else(|| value.clone());
                    }
                }
                value.clone()
            }
            Value::Bool(b) => Value::from(if *b { 1 } else { 0 }),
            _ => value.clone(),
        },
        "integer" => match value {
            Value::Null => Value::from(0),
            Value::String(s) if !s.trim().is_empty() => {
                if let Ok(parsed) = s.trim().parse::<f64>() {
                    if parsed.fract() == 0.0 && parsed.is_finite() {
                        return Value::from(parsed as i64);
                    }
                }
                value.clone()
            }
            Value::Bool(b) => Value::from(if *b { 1 } else { 0 }),
            _ => value.clone(),
        },
        "boolean" => match value {
            Value::Null => Value::Bool(false),
            Value::String(s) => match s.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => value.clone(),
            },
            Value::Number(n) => {
                if n.as_f64() == Some(1.0) {
                    Value::Bool(true)
                } else if n.as_f64() == Some(0.0) {
                    Value::Bool(false)
                } else {
                    value.clone()
                }
            }
            _ => value.clone(),
        },
        "string" => match value {
            Value::Null => Value::String(String::new()),
            Value::Number(n) => Value::String(n.to_string()),
            Value::Bool(b) => Value::String(b.to_string()),
            _ => value.clone(),
        },
        "null" => match value {
            Value::String(s) if s.is_empty() => Value::Null,
            Value::Number(n) if n.as_f64() == Some(0.0) => Value::Null,
            Value::Bool(false) => Value::Null,
            _ => value.clone(),
        },
        _ => value.clone(),
    }
}

// ----------------------------------------------------------------------------
// applySchemaObjectCoercion / applySchemaArrayCoercion
// ----------------------------------------------------------------------------

fn apply_schema_object_coercion(value: &mut Map<String, Value>, schema: &Value) {
    let Some(sobj) = schema.as_object() else {
        return;
    };
    let properties = sobj.get("properties").and_then(|p| p.as_object());
    let defined_keys: HashSet<&str> = properties
        .map(|p| p.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();

    if let Some(props) = properties {
        for (key, prop_schema) in props {
            if let Some(child) = value.get_mut(key) {
                let coerced = coerce_with_json_schema(child.clone(), prop_schema);
                *child = coerced;
            }
        }
    }

    if let Some(addl) = sobj.get("additionalProperties") {
        if let Some(addl_obj) = addl.as_object() {
            let addl_value = Value::Object(addl_obj.clone());
            for (key, child) in value.iter_mut() {
                if defined_keys.contains(key.as_str()) {
                    continue;
                }
                let coerced = coerce_with_json_schema(child.clone(), &addl_value);
                *child = coerced;
            }
        }
    }
}

fn apply_schema_array_coercion(value: &mut Vec<Value>, schema: &Value) {
    let Some(sobj) = schema.as_object() else {
        return;
    };
    match sobj.get("items") {
        Some(Value::Array(items)) => {
            for (i, item_schema) in items.iter().enumerate() {
                if i >= value.len() {
                    continue;
                }
                let coerced = coerce_with_json_schema(value[i].clone(), item_schema);
                value[i] = coerced;
            }
        }
        Some(items) if items.is_object() => {
            for entry in value.iter_mut() {
                let coerced = coerce_with_json_schema(entry.clone(), items);
                *entry = coerced;
            }
        }
        _ => {}
    }
}

// ----------------------------------------------------------------------------
// coerceWithUnionSchema
// ----------------------------------------------------------------------------

fn coerce_with_union_schema(value: &Value, schemas: &[Value]) -> Value {
    // Pass 1: if any member already validates, keep as-is.
    for schema in schemas {
        if let Some(v) = get_sub_schema_validator(schema) {
            if v.is_valid(value) {
                return value.clone();
            }
        }
    }
    // Pass 2: clone + coerce against each member; return first that validates.
    for schema in schemas {
        let candidate = coerce_with_json_schema(value.clone(), schema);
        if let Some(v) = get_sub_schema_validator(schema) {
            if v.is_valid(&candidate) {
                return candidate;
            }
        }
    }
    value.clone()
}

// ----------------------------------------------------------------------------
// coerceWithJsonSchema — the recursive entry point
// ----------------------------------------------------------------------------

/// Recursively coerce a value to conform to a JSON-Schema. Mirrors TS
/// `coerceWithJsonSchema`. Handles `allOf`/`anyOf`/`oneOf`, primitive-type
/// coercion, object-property recursion, and array-item recursion.
pub fn coerce_with_json_schema(value: Value, schema: &Value) -> Value {
    let mut next = value;

    if let Some(Value::Array(all_of)) = schema.as_object().and_then(|o| o.get("allOf")) {
        for nested in all_of {
            next = coerce_with_json_schema(next, nested);
        }
    }

    if let Some(Value::Array(any_of)) = schema.as_object().and_then(|o| o.get("anyOf")) {
        next = coerce_with_union_schema(&next, any_of);
    }

    if let Some(Value::Array(one_of)) = schema.as_object().and_then(|o| o.get("oneOf")) {
        next = coerce_with_union_schema(&next, one_of);
    }

    let types = schema_types(schema);
    let matches_union_member = types.len() > 1 && types.iter().any(|t| matches_json_type(&next, t));
    if !types.is_empty() && !matches_union_member {
        for ty in &types {
            let candidate = coerce_primitive_by_type(&next, ty);
            if candidate != next {
                next = candidate;
                break;
            }
        }
    }

    if types.iter().any(|t| t == "object") {
        if let Value::Object(ref mut map) = next {
            apply_schema_object_coercion(map, schema);
        }
    }

    if types.iter().any(|t| t == "array") {
        if let Value::Array(ref mut arr) = next {
            apply_schema_array_coercion(arr, schema);
        }
    }

    next
}

// ----------------------------------------------------------------------------
// normalizeOptionalNulls
// ----------------------------------------------------------------------------

/// Drop `null` values on non-required properties whose subschema rejects null.
/// Mirrors TS `normalizeOptionalNulls`. Recurses into arrays + object props.
fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    match value {
        Value::Array(arr) => {
            let items = schema.as_object().and_then(|o| o.get("items"));
            match items {
                Some(Value::Array(items)) => {
                    for (i, item_schema) in items.iter().enumerate() {
                        if i < arr.len() {
                            normalize_optional_nulls(&mut arr[i], item_schema);
                        }
                    }
                }
                Some(items) if items.is_object() => {
                    for entry in arr.iter_mut() {
                        normalize_optional_nulls(entry, items);
                    }
                }
                _ => {}
            }
            return;
        }
        Value::Object(map) => {
            let Some(sobj) = schema.as_object() else {
                return;
            };
            let Some(properties) = sobj.get("properties").and_then(|p| p.as_object()) else {
                return;
            };
            let required: HashSet<&str> = sobj
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            let mut to_remove: Vec<String> = Vec::new();
            for (key, prop_schema) in properties {
                let Some(child) = map.get_mut(key) else {
                    continue;
                };
                if child.is_null()
                    && !required.contains(key.as_str())
                    && !matches!(prop_schema.get("$ref"), Some(Value::String(_)))
                    && !sub_schema_accepts_null(prop_schema)
                {
                    to_remove.push(key.clone());
                } else {
                    normalize_optional_nulls(child, prop_schema);
                }
            }
            for key in to_remove {
                map.remove(&key);
            }
        }
        _ => {}
    }
}

/// True if `null` validates against the sub-schema (mirrors the inverted
/// `getSubSchemaValidator(schema)?.Check(null) === false` test).
fn sub_schema_accepts_null(schema: &Value) -> bool {
    if let Some(v) = get_sub_schema_validator(schema) {
        return v.is_valid(&Value::Null);
    }
    // No validator (malformed schema) — be permissive, matching the TS
    // short-circuit (`?.Check(null) === false` is falsy when validator is undefined).
    true
}

// ----------------------------------------------------------------------------
// Tool-argument normalization + validation
// ----------------------------------------------------------------------------

fn expects_object(schema: &Value) -> bool {
    let types = schema_types(schema);
    (types.iter().any(|ty| ty == "object") && !types.iter().any(|ty| ty == "string"))
        || schema
            .as_object()
            .is_some_and(|object| object.contains_key("properties"))
}

fn primary_string_property(schema: &Value) -> Option<&str> {
    let properties = schema.as_object()?.get("properties")?.as_object()?;
    ["path", "src", "file", "directory", "dir", "command", "query", "text", "pattern"]
        .into_iter()
        .find(|key| properties.contains_key(*key))
        .or_else(|| (properties.len() == 1).then(|| properties.keys().next().map(String::as_str)).flatten())
}

/// Normalize common LLM/provider wire-format deviations before JSON-Schema
/// validation. This intentionally only applies object-shape heuristics when
/// the tool schema expects an object; tools whose schema accepts strings retain
/// their original string semantics.
fn normalize_tool_arguments(mut args: Value, schema: &Value) -> Value {
    let object_schema = expects_object(schema);
    if object_schema {
        if let Value::String(raw) = &args {
            args = crate::json_parse::parse_terminal_tool_arguments(raw);
        }
    }

    if object_schema && args.is_null() {
        args = Value::Object(Map::new());
    }
    if object_schema {
        args = match args {
            Value::String(text) => {
                let mut object = Map::new();
                if let Some(key) = primary_string_property(schema) {
                    object.insert(key.to_owned(), Value::String(text));
                }
                Value::Object(object)
            }
            other => other,
        };
    }

    // Common semantic aliases are applied only when the canonical key appears
    // in this tool's schema and is absent from the supplied object.
    if let (Value::Object(arguments), Some(properties)) = (
        &mut args,
        schema.as_object().and_then(|object| object.get("properties")).and_then(Value::as_object),
    ) {
        for (alias, canonical) in [
            ("dir", "path"),
            ("directory", "path"),
            ("folder", "path"),
            ("file", "path"),
            ("src", "path"),
        ] {
            if properties.contains_key(canonical)
                && !arguments.contains_key(canonical)
                && arguments.contains_key(alias)
            {
                if let Some(value) = arguments.remove(alias) {
                    arguments.insert(canonical.to_owned(), value);
                }
            }
        }
    }
    args
}

fn example_value(schema: &Value) -> Value {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if let Some(first) = values.first() {
            return first.clone();
        }
    }
    match schema_types(schema).first().map(String::as_str) {
        Some("object") => {
            let mut object = Map::new();
            let required: HashSet<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, property) in properties {
                    if required.contains(key.as_str()) {
                        object.insert(key.clone(), example_value(property));
                    }
                }
            }
            Value::Object(object)
        }
        Some("array") => Value::Array(Vec::new()),
        Some("string") => Value::String(String::new()),
        Some("integer") | Some("number") => Value::from(0),
        Some("boolean") => Value::Bool(false),
        _ => Value::Object(Map::new()),
    }
}

/// Validate (and coerce) tool-call arguments against the tool's parameter
/// schema. Mirrors TS `validateToolArguments`:
///
/// 1. `normalize_optional_nulls` — drop null on non-required props that reject null.
/// 2. `coerce_with_json_schema` — LLM-sloppy-JSON tolerance.
/// 3. `JSONSchema::is_valid` — authoritative check.
///
/// Returns the coerced arguments on success, or `AiError::Schema` with a
/// formatted message (path + message per error + the received JSON) on failure.
pub fn validate_tool_arguments(tool: &Tool, tool_call: &ToolCall) -> Result<Value, AiError> {
    let raw_arguments = &tool_call.arguments;
    tracing::debug!(tool = %tool.name, arguments = ?raw_arguments, "normalizing tool arguments");
    let mut args = normalize_tool_arguments(raw_arguments.clone(), tool.parameters.as_value());

    // Step 1: normalize optional nulls.
    normalize_optional_nulls(&mut args, tool.parameters.as_value());

    // Step 2: coerce. TS only skips the custom coerce pass when the schema
    // carries the TypeBox Kind symbol; schemars produces plain JSON Schemas, so
    // the Rust port always runs it.
    let coerced = coerce_with_json_schema(args, tool.parameters.as_value());

    // Step 3: authoritative validate.
    let validator = match get_sub_schema_validator(tool.parameters.as_value()) {
        Some(v) => v,
        None => {
            let error = JSONSchema::compile(tool.parameters.as_value())
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "validator unavailable".to_string());
            return Err(AiError::Schema {
                message: format!("could not compile tool schema for {:?}: {error}", tool.name),
            });
        }
    };

    if validator.is_valid(&coerced) {
        return Ok(coerced);
    }

    let errors: Vec<String> = collect_validation_errors(&validator, &coerced);
    let errors_block = if errors.is_empty() {
        "Unknown validation error".to_string()
    } else {
        errors
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    Err(AiError::Schema {
        message: format!(
            "Validation failed for tool {:?}:\n{errors_block}\n\nReceived arguments:\n{}\n\nExpected parameter schema:\n{}\n\nOne valid example:\n{}",
            tool_call.name,
            serde_json::to_string_pretty(&tool_call.arguments)
                .unwrap_or_else(|_| "<unprintable>".into()),
            serde_json::to_string_pretty(tool.parameters.as_value())
                .unwrap_or_else(|_| "<unavailable>".into()),
            serde_json::to_string_pretty(&example_value(tool.parameters.as_value()))
                .unwrap_or_else(|_| "{}".into())
        ),
    })
}

/// Find a tool by name in a slice and validate its call. Mirrors TS
/// `validateToolCall`. Returns `Err(AiError::Schema)` if the tool isn't found.
pub fn validate_tool_call(tools: &[Tool], tool_call: &ToolCall) -> Result<Value, AiError> {
    let tool = tools
        .iter()
        .find(|t| t.name == tool_call.name)
        .ok_or_else(|| AiError::Schema {
            message: format!("Tool {:?} not found", tool_call.name),
        })?;
    validate_tool_arguments(tool, tool_call)
}

/// Collect human-readable validation errors with JSON-pointer paths, mirroring
/// the TS `validator.Errors(...).map(formatValidationPath)` output. Uses
/// `JSONSchema::validate`, whose `Err` variant is an iterator over all errors.
fn collect_validation_errors(validator: &JSONSchema, value: &Value) -> Vec<String> {
    let Err(errors) = validator.validate(value) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for error in errors {
        // Build a dotted path from the JSONPointer chunks (Property/Index),
        // matching the TS `formatValidationPath` output; root → "root".
        let dotted: String = error
            .instance_path
            .iter()
            .map(|chunk| match chunk {
                PathChunk::Property(p) => p.as_ref().to_string(),
                PathChunk::Index(i) => i.to_string(),
                PathChunk::Keyword(k) => k.to_string(),
            })
            .collect::<Vec<_>>()
            .join(".");
        let label = if dotted.is_empty() {
            "root".to_string()
        } else {
            dotted
        };
        out.push(format!("{label}: {error}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tool, ToolCall, ToolCallType};
    use serde_json::json;

    fn tool(name: &str, schema: Value) -> Tool {
        Tool {
            name: name.to_string(),
            description: String::new(),
            parameters: crate::types::Schema::new(schema),
            constrained_sampling: None,
        }
    }

    fn call(args: Value) -> ToolCall {
        ToolCall {
            kind: ToolCallType,
            id: "id".to_string(),
            name: "t".to_string(),
            arguments: args,
            thought_signature: None,
            namespace: None,
            recorded: None,
        }
    }

    #[test]
    fn invalid_tool_schema_rejects_arguments() {
        let t = tool("t", json!({"type": "not-a-json-schema-type"}));
        let error = validate_tool_arguments(&t, &call(json!({}))).unwrap_err();
        assert!(error.to_string().contains("could not compile tool schema"));
    }

    #[test]
    fn validator_cache_uses_content_after_schema_changes() {
        let mut cache = ValidatorCache::default();
        let mut schema = json!({"type": "integer", "minimum": 1});
        let original = cache.insert(
            &schema,
            schema_fingerprint(&schema),
            Arc::new(JSONSchema::compile(&schema).unwrap()),
        );
        let same_content = cache
            .get(&schema.clone(), schema_fingerprint(&schema))
            .unwrap();
        assert!(Arc::ptr_eq(&original, &same_content));
        assert!(original.is_valid(&json!(2)));

        schema["minimum"] = json!(3);
        assert!(cache.get(&schema, schema_fingerprint(&schema)).is_none());
        let changed = cache.insert(
            &schema,
            schema_fingerprint(&schema),
            Arc::new(JSONSchema::compile(&schema).unwrap()),
        );
        assert!(!Arc::ptr_eq(&original, &changed));
        assert!(!changed.is_valid(&json!(2)));
        assert!(changed.is_valid(&json!(3)));
    }

    #[test]
    fn validator_fingerprint_collisions_do_not_reuse_wrong_schema() {
        let mut cache = ValidatorCache::default();
        let integer = json!({"type": "integer"});
        let string = json!({"type": "string"});
        cache.insert(
            &integer,
            7,
            Arc::new(JSONSchema::compile(&integer).unwrap()),
        );
        assert!(cache.get(&string, 7).is_none());
        cache.insert(&string, 7, Arc::new(JSONSchema::compile(&string).unwrap()));
        assert!(!cache.get(&integer, 7).unwrap().is_valid(&json!("text")));
        assert!(!cache.get(&string, 7).unwrap().is_valid(&json!(42)));
    }

    #[test]
    fn validation_observes_replaced_tool_schema() {
        let mut t = tool("t", json!({"type": "integer", "minimum": 1}));
        let tc = call(json!(2));
        assert_eq!(validate_tool_arguments(&t, &tc).unwrap(), json!(2));
        t.parameters = crate::types::Schema::new(json!({"type": "integer", "minimum": 3}));
        assert!(validate_tool_arguments(&t, &tc).is_err());
    }

    #[test]
    fn validator_cache_evicts_oldest_content_at_capacity() {
        let mut cache = ValidatorCache::default();
        let validator = Arc::new(JSONSchema::compile(&json!({"type": "integer"})).unwrap());
        for minimum in 0..=VALIDATOR_CACHE_CAPACITY {
            let schema = json!({"minimum": minimum});
            cache.insert(&schema, schema_fingerprint(&schema), validator.clone());
        }
        assert_eq!(cache.validators.len(), VALIDATOR_CACHE_CAPACITY);
        let evicted = json!({"minimum": 0});
        let retained = json!({"minimum": 1});
        assert!(cache.get(&evicted, schema_fingerprint(&evicted)).is_none());
        assert!(cache
            .get(&retained, schema_fingerprint(&retained))
            .is_some());
    }

    #[test]
    fn object_tool_normalizes_common_wire_argument_shapes() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"}
            }
        });
        let t = tool("ls", schema.clone());
        let raw_cases = [
            ("", json!({})),
            ("null", json!({})),
            ("{}", json!({})),
            (r#"```json
{"path":"."}
```"#, json!({"path":"."})),
            (r#"{"path":".""#, json!({"path":"."})),
            (r#"{"path":".",}"#, json!({"path":"."})),
            (r#"{'path':'.'}"#, json!({"path":"."})),
            (r#""{\"path\":\".\"}""#, json!({"path":"."})),
            (".", json!({"path":"."})),
        ];
        for (raw, expected) in &raw_cases {
            assert_eq!(
                normalize_tool_arguments(json!(raw), &schema),
                expected.clone(),
                "normalizing {raw:?}"
            );
        }
        assert_eq!(
            normalize_tool_arguments(json!({"dir":"."}), &schema),
            json!({"path":"."})
        );
        for (raw, _) in &raw_cases {
            assert!(validate_tool_arguments(&t, &call(json!(raw))).is_ok(), "{raw:?}");
        }
        assert!(validate_tool_arguments(&t, &call(json!({"dir":"."}))).is_ok());
    }

    #[test]
    fn validation_errors_include_model_friendly_schema_and_example() {
        let t = tool(
            "required",
            json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        );
        let message = validate_tool_arguments(&t, &call(json!({})))
            .unwrap_err()
            .to_string();
        assert!(message.contains("Expected parameter schema"));
        assert!(message.contains("One valid example"));
        assert!(message.contains(r#""name": """#));
    }

    #[test]
    fn string_tool_arguments_are_not_mistaken_for_malformed_json() {
        for schema in [
            json!({"type": "string"}),
            json!({"type": ["object", "string"]}),
            json!({"anyOf": [{"type": "object"}, {"type": "string"}]}),
        ] {
            let t = tool("t", schema);
            for raw in ["", "{\"payload\":", "ordinary raw text"] {
                assert_eq!(
                    validate_tool_arguments(&t, &call(json!(raw))).unwrap(),
                    json!(raw)
                );
            }
        }
    }

    #[test]
    fn coerce_string_number_to_number() {
        let t = tool(
            "t",
            json!({"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}),
        );
        let tc = call(json!({"n":"42"}));
        let v = validate_tool_arguments(&t, &tc).unwrap();
        assert_eq!(v["n"], json!(42));
    }

    #[test]
    fn coerce_string_integer_to_integer() {
        let t = tool(
            "t",
            json!({"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]}),
        );
        let tc = call(json!({"n":"7"}));
        let v = validate_tool_arguments(&t, &tc).unwrap();
        assert_eq!(v["n"], json!(7));
    }

    #[test]
    fn coerce_string_bool_to_boolean() {
        let t = tool(
            "t",
            json!({"type":"object","properties":{"b":{"type":"boolean"}},"required":["b"]}),
        );
        let tc = call(json!({"b":"true"}));
        let v = validate_tool_arguments(&t, &tc).unwrap();
        assert_eq!(v["b"], json!(true));
    }

    #[test]
    fn coerce_number_to_string() {
        let t = tool(
            "t",
            json!({"type":"object","properties":{"s":{"type":"string"}},"required":["s"]}),
        );
        let tc = call(json!({"s":42}));
        let v = validate_tool_arguments(&t, &tc).unwrap();
        assert_eq!(v["s"], json!("42"));
    }

    #[test]
    fn normalize_optional_null_dropped() {
        let t = tool(
            "t",
            json!({"type":"object","properties":{"opt":{"type":"string"}},"required":[]}),
        );
        let tc = call(json!({"opt":null}));
        let v = validate_tool_arguments(&t, &tc).unwrap();
        assert!(v.get("opt").is_none());
    }

    #[test]
    fn union_keeps_valid_member() {
        let t = tool(
            "t",
            json!({"type":"object","properties":{"x":{"oneOf":[{"type":"string"},{"type":"number"}]}},"required":["x"]}),
        );
        let tc = call(json!({"x":5}));
        let v = validate_tool_arguments(&t, &tc).unwrap();
        assert_eq!(v["x"], json!(5));
    }

    #[test]
    fn union_coerces_to_member() {
        let t = tool(
            "t",
            json!({"type":"object","properties":{"x":{"oneOf":[{"type":"boolean"},{"type":"string"}]}},"required":["x"]}),
        );
        let tc = call(json!({"x":1}));
        let v = validate_tool_arguments(&t, &tc).unwrap();
        assert_eq!(v["x"], json!(true));
    }

    #[test]
    fn rejects_uncoercible() {
        let t = tool(
            "t",
            json!({"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}),
        );
        let tc = call(json!({"n":"not-a-number"}));
        assert!(validate_tool_arguments(&t, &tc).is_err());
    }

    #[test]
    fn validate_tool_call_finds_named_tool() {
        let tools = vec![
            tool("other", json!({"type":"object"})),
            tool(
                "t",
                json!({"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}),
            ),
        ];
        let tc = ToolCall {
            kind: ToolCallType,
            id: "id".into(),
            name: "t".into(),
            arguments: json!({"n":"9"}),
            thought_signature: None,
            namespace: None,
            recorded: None,
        };
        let v = validate_tool_call(&tools, &tc).unwrap();
        assert_eq!(v["n"], json!(9));
    }

    #[test]
    fn validate_tool_call_missing_tool_errors() {
        let tools = vec![tool("other", json!({"type":"object"}))];
        let tc = ToolCall {
            kind: ToolCallType,
            id: "id".into(),
            name: "absent".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
            recorded: None,
        };
        let err = validate_tool_call(&tools, &tc).unwrap_err();
        assert!(matches!(err, AiError::Schema { .. }));
    }
}
