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
use std::collections::HashSet;
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
// Validator cache — one compiled `JSONSchema` per schema, keyed by the
// serialized schema content. Mirrors the TS `WeakMap<object, Compile>` cache.
// ----------------------------------------------------------------------------

static VALIDATOR_CACHE: Mutex<Option<std::collections::HashMap<String, Arc<JSONSchema>>>> =
    Mutex::new(None);

/// Compile-or-fetch a validator for a schema; `None` if compilation fails
/// (mirrors TS `getSubSchemaValidator` returning `undefined` on throw).
fn get_sub_schema_validator(schema: &Value) -> Option<Arc<JSONSchema>> {
    let key = schema.to_string();
    let mut guard = VALIDATOR_CACHE.lock().unwrap();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    if let Some(v) = map.get(&key) {
        return Some(v.clone());
    }
    // jsonschema's default resolver handles `$ref` inside self-contained
    // schemas; compilation can fail on malformed input — in that case we cache
    // nothing and return None, matching the TS try/catch → undefined.
    match JSONSchema::compile(schema) {
        Ok(v) => {
            let arc = Arc::new(v);
            map.insert(key, arc.clone());
            Some(arc)
        }
        Err(_) => None,
    }
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
// validateToolArguments — public entry point
// ----------------------------------------------------------------------------

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
    let mut args = tool_call.arguments.clone();

    // Step 1: normalize optional nulls.
    normalize_optional_nulls(&mut args, tool.parameters.as_value());

    // Step 2: coerce. TS only skips the custom coerce pass when the schema
    // carries the TypeBox Kind symbol; schemars produces plain JSON Schemas, so
    // the Rust port always runs it.
    let coerced = coerce_with_json_schema(args.clone(), tool.parameters.as_value());

    // Step 3: authoritative validate.
    let validator = match get_sub_schema_validator(tool.parameters.as_value()) {
        Some(v) => v,
        None => match JSONSchema::compile(&Value::Bool(true)) {
            Ok(v) => Arc::new(v),
            // Truly unreachable: `true` is a valid boolean schema.
            Err(e) => {
                return Err(AiError::Schema {
                    message: format!("could not compile tool schema for {:?}: {e}", tool.name),
                })
            }
        },
    };

    let check_value = if coerced != args { &coerced } else { &args };
    if validator.is_valid(check_value) {
        return Ok(check_value.clone());
    }

    let errors: Vec<String> = collect_validation_errors(&validator, check_value);
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
            "Validation failed for tool {:?}:\n{errors_block}\n\nReceived arguments:\n{}",
            tool_call.name,
            serde_json::to_string_pretty(&tool_call.arguments)
                .unwrap_or_else(|_| "<unprintable>".into())
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
