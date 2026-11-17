//! JSON-Schema → strict-subset transform, shared by providers that ask the
//! model for strict/structured tool arguments.
//!
//! Extracted from `providers/anthropic/build_params.rs` so the OpenAI
//! providers can reuse it. OpenAI's `strict: true` rejects any object node
//! without `additionalProperties: false` — exactly what `schemars` omits for
//! the built-in tools (`read`, `bash`, `edit`, `write`). Sending `strict:
//! true` with an untransformed schema is a hard HTTP 400, so callers must
//! only enable strict mode when this returns `Ok`.

use serde_json::{json, Value};
use std::collections::BTreeSet;

use crate::types::{ConstrainedSamplingConfig, Tool};

const UNSUPPORTED_STRICT_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn is_object(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

fn schema_allows_null(schema: &Value) -> bool {
    if !is_object(schema) {
        return false;
    }
    let obj = schema.as_object().unwrap();
    if let Some(t) = obj.get("type") {
        match t {
            Value::String(s) if s == "null" => return true,
            Value::Array(arr) if arr.iter().any(|v| v.as_str() == Some("null")) => return true,
            _ => {}
        }
    }
    if obj.get("const").and_then(|v| v.as_null()).is_some() {
        return true;
    }
    if let Some(arr) = obj.get("enum").and_then(|v| v.as_array()) {
        if arr.iter().any(|v| v.is_null()) {
            return true;
        }
    }
    if let Some(any) = obj.get("anyOf").and_then(|v| v.as_array()) {
        return any.iter().any(schema_allows_null);
    }
    false
}

fn is_structured(schema: &Value) -> bool {
    if !is_object(schema) {
        return false;
    }
    let obj = schema.as_object().unwrap();
    let types: Vec<String> = match obj.get("type") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => vec![],
    };
    types.iter().any(|t| t == "object" || t == "array")
        || obj.contains_key("properties")
        || obj.contains_key("items")
}

/// Recursively transform a JSON Schema into the strict subset both Anthropic
/// and OpenAI accept. Mirrors `makeJsonSchemaNodeStrict`. Returns `Err(())`
/// when a construct the strict mode rejects is present (the caller falls back
/// to the non-strict schema).
pub(crate) fn make_strict_json_schema(schema: &Value) -> Result<Value, ()> {
    let mut cloned = schema.clone();
    make_strict_node(&mut cloned)?;
    if !matches!(cloned.get("type"), Some(Value::String(s)) if s == "object") {
        return Err(());
    }
    Ok(cloned)
}

fn make_strict_node(schema: &mut Value) -> Result<(), ()> {
    let obj = match schema.as_object_mut() {
        Some(o) => o,
        None => return Err(()),
    };
    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if obj.contains_key(*key) {
            return Err(());
        }
    }

    if let Some(any_of) = obj.get_mut("anyOf") {
        let arr = any_of.as_array_mut().ok_or(())?;
        if arr.is_empty() {
            return Err(());
        }
        for variant in arr.iter_mut() {
            if is_structured(variant) {
                return Err(());
            }
            make_strict_node(variant)?;
        }
    }

    if let Some(items) = obj.get_mut("items") {
        if items.is_array() {
            // tuple schemas unsupported.
            return Err(());
        }
        make_strict_node(items)?;
    }

    let is_object_schema = matches!(obj.get("type"), Some(Value::String(s)) if s == "object");
    if obj.contains_key("properties") && !is_object_schema {
        return Err(());
    }
    if !is_object_schema {
        return Ok(());
    }
    if let Some(ap) = obj.get("additionalProperties") {
        // Only `false` is permitted in strict mode; `true` or a schema object
        // is unsupported.
        if !matches!(ap, Value::Bool(false)) {
            return Err(());
        }
    }
    // Snapshot `required` from the object first so the mutable borrow of
    // `properties` below doesn't conflict with reading `required` later.
    let required_set: BTreeSet<String> = match obj.get("required") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => BTreeSet::new(),
    };
    let Some(properties_val) = obj.get_mut("properties") else {
        // No properties — still need required=[] + additionalProperties=false.
        obj.insert("required".into(), Value::Array(Vec::new()));
        obj.insert("additionalProperties".into(), Value::Bool(false));
        return Ok(());
    };
    if !properties_val.is_object() {
        return Err(());
    }
    let prop_obj = properties_val.as_object_mut().unwrap();
    let property_names: Vec<String> = prop_obj.keys().cloned().collect();
    for name in &property_names {
        if !required_set.contains(name) {
            let prop = prop_obj.get_mut(name).unwrap();
            if !schema_allows_null(prop) {
                // Wrap in anyOf [prop, {type:null}].
                let original = prop.clone();
                *prop = json!({ "anyOf": [original, { "type": "null" }] });
            }
        }
        // Recurse into each property.
        let prop = prop_obj.get_mut(name).unwrap();
        make_strict_node(prop)?;
    }
    obj.insert(
        "required".into(),
        Value::Array(property_names.into_iter().map(Value::String).collect()),
    );
    obj.insert("additionalProperties".into(), Value::Bool(false));
    Ok(())
}

/// Strict parameters for `tool`, or `None` when the tool did not opt in via
/// `constrained_sampling` or its schema uses a construct strict mode rejects.
///
/// Callers must leave `strict` off when this returns `None`. Advertising
/// `strict: true` alongside an untransformed schema is a hard HTTP 400 from
/// OpenAI (`'additionalProperties' is required to be supplied and to be
/// false`), which is how the built-in tools used to fail.
pub(crate) fn strict_tool_parameters(tool: &Tool) -> Option<Value> {
    let config = tool.constrained_sampling.as_ref()?;
    if !matches!(config, ConstrainedSamplingConfig::JsonSchema { .. }) {
        return None;
    }
    make_strict_json_schema(tool.parameters.as_value()).ok()
}
