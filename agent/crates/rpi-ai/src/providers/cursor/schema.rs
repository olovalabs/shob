//! Cursor's constrained MCP catalog rejects composition keywords. Project only
//! the advertised copy; host-side validation retains the canonical tool schema.
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashSet};

pub(super) fn project(schema: Value) -> Value {
    walk(schema.clone(), &schema, &mut HashSet::new(), false)
}
fn needs_projection(value: &Value, schema_map: bool) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|v| needs_projection(v, false)),
        Value::Object(map) => map.iter().any(|(key, value)| {
            if schema_map {
                return needs_projection(value, false);
            }
            if matches!(key.as_str(), "$ref" | "anyOf" | "oneOf" | "allOf") {
                return true;
            }
            match key.as_str() {
                "properties" | "patternProperties" | "$defs" | "definitions"
                | "dependentSchemas" => needs_projection(value, true),
                "items"
                | "prefixItems"
                | "additionalProperties"
                | "additionalItems"
                | "contains"
                | "propertyNames"
                | "not"
                | "if"
                | "then"
                | "else" => needs_projection(value, false),
                _ => false,
            }
        }),
        _ => false,
    }
}
fn walk(value: Value, root: &Value, refs: &mut HashSet<String>, schema_map: bool) -> Value {
    let Value::Object(mut map) = value else {
        return match value {
            Value::Array(values) => Value::Array(
                values
                    .into_iter()
                    .map(|v| walk(v, root, refs, false))
                    .collect(),
            ),
            other => other,
        };
    };
    if schema_map {
        return Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, walk(value, root, refs, false)))
                .collect(),
        );
    }
    if let Some(Value::String(reference)) = map.remove("$ref") {
        if refs.insert(reference.clone()) {
            if let Some(target) = reference
                .strip_prefix('#')
                .and_then(|pointer| root.pointer(pointer))
            {
                if let Value::Object(mut resolved) = walk(target.clone(), root, refs, false) {
                    resolved.extend(map);
                    map = resolved;
                }
            }
            refs.remove(&reference);
        }
    }
    // Widening a negative schema or a conditional test can narrow the outer
    // schema. Drop that restriction as a whole instead of inverting the change.
    if map.get("not").is_some_and(|v| needs_projection(v, false)) {
        map.remove("not");
    }
    if map.get("if").is_some_and(|v| needs_projection(v, false)) {
        for key in ["if", "then", "else"] {
            map.remove(key);
        }
    }
    for (key, value) in map.iter_mut() {
        let kind = match key.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                Some(true)
            }
            "items"
            | "prefixItems"
            | "additionalProperties"
            | "additionalItems"
            | "contains"
            | "propertyNames"
            | "not"
            | "if"
            | "then"
            | "else"
            | "anyOf"
            | "oneOf"
            | "allOf" => Some(false),
            _ => None,
        };
        if let Some(schema_map) = kind {
            *value = walk(std::mem::take(value), root, refs, schema_map);
        }
    }
    for combiner in ["anyOf", "oneOf", "allOf"] {
        let Some(Value::Array(branches)) = map.remove(combiner) else {
            continue;
        };
        if branches.is_empty()
            || !branches.iter().all(|branch| {
                branch.as_object().is_some_and(|branch| {
                    branch.get("type").and_then(Value::as_str) == Some("object")
                        || branch.contains_key("properties")
                })
            })
        {
            // Scalar unions cannot be represented by this catalog. Keeping the
            // surrounding constraints while dropping the union only widens it.
            continue;
        }
        let conjunction = combiner == "allOf";
        let mut names = BTreeSet::new();
        for branch in &branches {
            if let Some(properties) = branch.get("properties").and_then(Value::as_object) {
                names.extend(properties.keys().cloned());
            }
        }
        let mut properties = map
            .remove("properties")
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        for name in names {
            if properties.contains_key(&name) {
                continue;
            }
            let mut candidates = Vec::new();
            for branch in &branches {
                if let Some(property) = branch.get("properties").and_then(|v| v.get(&name)) {
                    candidates.push(property.clone());
                } else if !conjunction {
                    match branch.get("additionalProperties") {
                        Some(Value::Bool(false)) => {}
                        Some(Value::Object(schema)) => {
                            candidates.push(Value::Object(schema.clone()))
                        }
                        _ => candidates.push(Value::Object(Map::new())),
                    }
                }
            }
            let mut merged = Map::new();
            if conjunction {
                for candidate in candidates {
                    if let Value::Object(candidate) = candidate {
                        for (key, value) in candidate {
                            merged.entry(key).or_insert(value);
                        }
                    }
                }
            } else if let Some(Value::Object(first)) = candidates.first() {
                // Constraints common to all branches describe a sound union;
                // contradictory branch-specific constraints must not survive.
                for (key, value) in first {
                    if candidates.iter().all(|v| v.get(key) == Some(value)) {
                        merged.insert(key.clone(), value.clone());
                    }
                }
            }
            properties.insert(name, Value::Object(merged));
        }
        if !properties.is_empty() {
            map.insert("properties".into(), Value::Object(properties));
        }
        let requirements: Vec<BTreeSet<String>> = branches
            .iter()
            .map(|branch| {
                branch
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .collect();
        let mut required: BTreeSet<String> = map
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if conjunction {
            for branch in requirements {
                required.extend(branch);
            }
        } else if let Some(first) = requirements.first() {
            for name in first {
                if requirements.iter().all(|branch| branch.contains(name)) {
                    required.insert(name.clone());
                }
            }
        }
        if !required.is_empty() {
            map.insert(
                "required".into(),
                Value::Array(required.into_iter().map(Value::String).collect()),
            );
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn projects_object_unions_and_resolves_local_references() {
        let schema = json!({"$defs":{"name":{"type":"string"}},"type":"object","properties":{"anyOf":{"$ref":"#/$defs/name"}},"anyOf":[{"type":"object","properties":{"a":{"type":"string"}},"required":["a"]},{"type":"object","properties":{"b":{"type":"integer"}},"required":["b"]}]});
        let result = project(schema.clone());
        assert!(result.get("anyOf").is_none());
        assert_eq!(result["properties"]["anyOf"]["type"], "string");
        assert!(result["properties"].get("a").is_some());
        assert!(result["properties"].get("b").is_some());
        assert!(result.get("required").is_none());
        assert!(
            schema.get("anyOf").is_some(),
            "canonical schema remains untouched"
        );
    }
    #[test]
    fn negated_unions_are_removed_as_a_whole() {
        let result =
            project(json!({"type":"string","not":{"anyOf":[{"const":"a"},{"const":"b"}]}}));
        assert!(result.get("not").is_none());
        assert_eq!(result["type"], "string");
    }
}
