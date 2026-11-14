//! Integration tests for `coerce_with_json_schema` + `validate_tool_arguments`.
//! Mirrors the table-driven cases in `validation.ts`'s test block. Lives in
//! `tests/` so it exercises the crate's public API exactly the way `pi-agent`
//! and `pi-tools` will.

use rpi_ai::schema::{coerce_with_json_schema, validate_tool_arguments, validate_tool_call};
use rpi_ai::{AiError, Schema, Tool, ToolCall, ToolCallType};
use serde_json::{json, Value};

fn tool(name: &str, schema: Value) -> Tool {
    Tool {
        name: name.to_string(),
        description: String::new(),
        parameters: Schema::new(schema),
        constrained_sampling: None,
    }
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        kind: ToolCallType,
        id: "id".to_string(),
        name: name.to_string(),
        arguments: args,
        thought_signature: None,
        namespace: None,
        recorded: None,
    }
}

// --- primitive coercions -----------------------------------------------------

#[test]
fn string_to_number() {
    let v = coerce_with_json_schema(json!("42"), &json!({"type":"number"}));
    assert_eq!(v, json!(42));
}

#[test]
fn string_to_integer() {
    let v = coerce_with_json_schema(json!("7"), &json!({"type":"integer"}));
    assert_eq!(v, json!(7));
}

#[test]
fn string_true_to_boolean() {
    let v = coerce_with_json_schema(json!("true"), &json!({"type":"boolean"}));
    assert_eq!(v, json!(true));
}

#[test]
fn number_one_to_boolean() {
    let v = coerce_with_json_schema(json!(1), &json!({"type":"boolean"}));
    assert_eq!(v, json!(true));
}

#[test]
fn number_to_string() {
    let v = coerce_with_json_schema(json!(42), &json!({"type":"string"}));
    assert_eq!(v, json!("42"));
}

#[test]
fn bool_to_string() {
    let v = coerce_with_json_schema(json!(true), &json!({"type":"string"}));
    assert_eq!(v, json!("true"));
}

#[test]
fn null_to_number_is_zero() {
    let v = coerce_with_json_schema(json!(null), &json!({"type":"number"}));
    assert_eq!(v, json!(0));
}

#[test]
fn null_to_boolean_is_false() {
    let v = coerce_with_json_schema(json!(null), &json!({"type":"boolean"}));
    assert_eq!(v, json!(false));
}

#[test]
fn null_to_string_is_empty() {
    let v = coerce_with_json_schema(json!(null), &json!({"type":"string"}));
    assert_eq!(v, json!(""));
}

#[test]
fn empty_string_to_null() {
    let v = coerce_with_json_schema(json!(""), &json!({"type":"null"}));
    assert_eq!(v, json!(null));
}

// --- object / array recursion ------------------------------------------------

#[test]
fn object_property_coercion() {
    let v = coerce_with_json_schema(
        json!({"n":"42","b":"true"}),
        &json!({"type":"object","properties":{"n":{"type":"number"},"b":{"type":"boolean"}}}),
    );
    assert_eq!(v, json!({"n":42,"b":true}));
}

#[test]
fn array_item_coercion() {
    let v = coerce_with_json_schema(
        json!(["1", "2", "3"]),
        &json!({"type":"array","items":{"type":"integer"}}),
    );
    assert_eq!(v, json!([1, 2, 3]));
}

#[test]
fn nested_object_array() {
    let v = coerce_with_json_schema(
        json!({"items":[{"n":"1"},{"n":"2"}]}),
        &json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"n":{"type":"number"}}}}}}),
    );
    assert_eq!(v, json!({"items":[{"n":1},{"n":2}]}));
}

// --- normalize_optional_nulls (via the public entry point) -------------------

#[test]
fn optional_null_dropped_required_null_kept_or_coerced() {
    // Optional string + null → dropped.
    let t = tool(
        "t",
        json!({"type":"object","properties":{"opt":{"type":"string"},"req":{"type":"string"}},"required":["req"]}),
    );
    let tc = call("t", json!({"opt":null,"req":null}));
    let v = validate_tool_arguments(&t, &tc).unwrap();
    // `opt` (optional, null-rejecting) is dropped; `req` is coerced to "" (the
    // string/null coercion) since it's required and can't simply be removed.
    assert!(v.get("opt").is_none());
    assert_eq!(v["req"], json!(""));
}

// --- unions ------------------------------------------------------------------

#[test]
fn union_keeps_already_valid_member() {
    let v = coerce_with_json_schema(
        json!(5),
        &json!({"oneOf":[{"type":"string"},{"type":"number"}]}),
    );
    assert_eq!(v, json!(5));
}

#[test]
fn union_coerces_to_validating_member() {
    let v = coerce_with_json_schema(
        json!(1),
        &json!({"oneOf":[{"type":"boolean"},{"type":"string"}]}),
    );
    assert_eq!(v, json!(true));
}

// --- validation failures -----------------------------------------------------

#[test]
fn uncoercible_value_rejected() {
    let t = tool(
        "t",
        json!({"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}),
    );
    let tc = call("t", json!({"n":"not-a-number"}));
    let err = validate_tool_arguments(&t, &tc).unwrap_err();
    assert!(matches!(err, AiError::Schema { .. }));
}

#[test]
fn missing_required_rejected() {
    let t = tool(
        "t",
        json!({"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}),
    );
    let tc = call("t", json!({}));
    assert!(validate_tool_arguments(&t, &tc).is_err());
}

#[test]
fn validate_tool_call_missing_tool_errors() {
    let tools = vec![tool("other", json!({"type":"object"}))];
    let tc = call("absent", json!({}));
    let err = validate_tool_call(&tools, &tc).unwrap_err();
    assert!(matches!(err, AiError::Schema { .. }));
}

#[test]
fn validate_tool_call_dispatches_to_named_tool() {
    let tools = vec![
        tool("other", json!({"type":"object"})),
        tool(
            "t",
            json!({"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}),
        ),
    ];
    let tc = call("t", json!({"n":"9"}));
    let v = validate_tool_call(&tools, &tc).unwrap();
    assert_eq!(v["n"], json!(9));
}
