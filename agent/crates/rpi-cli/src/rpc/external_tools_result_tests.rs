#[test]
fn structured_object_is_explicit_content_without_metadata_injection() {
    let source = r#"{
        "content":[{"type":"text","text":"answer","annotations":{"original":true}}],
        "structuredContent":{"nested":{"values":[1,true,null,"雪"]},"empty":{}},
        "_meta":{"private":"metadata must not reach provider"},
        "toolName":"not_a_registered_tool",
        "inputSchema":{"type":"object","secret":"schema must not reach provider"},
        "added_tool_names":["injected"],
        "terminate":true
    }"#;
    let raw: Value = serde_json::from_str(source).unwrap();
    let result = decode_result(raw.clone()).unwrap();
    assert_eq!(result.details, json!({"externalToolResult": raw}));
    assert_eq!(result.content.len(), 2);
    assert_eq!(result.content[0], TextContentOrImage::text("answer"));
    let TextContentOrImage::Text(projected) = &result.content[1] else {
        panic!("structured result must be an explicit text block");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&projected.text).unwrap(),
        raw["structuredContent"]
    );
    assert!(result.added_tool_names.is_empty());
    assert!(!result.terminate);
    // Details retain every original field, including annotations not represented
    // by native provider content; decoding never rewrites the source result.
    assert_eq!(
        result.details["externalToolResult"],
        serde_json::from_str::<Value>(source).unwrap()
    );
}

#[test]
fn equivalent_json_text_is_not_duplicated_or_rewritten() {
    let text = " \n{\"b\": [2, 3], \"a\": {\"x\": true}} \n";
    let raw = json!({
        "content":[{"type":"text","text":"other"},{"type":"text","text":text}],
        "structuredContent":{"a":{"x":true},"b":[2,3]}
    });
    let result = decode_result(raw.clone()).unwrap();
    assert_eq!(
        result.content,
        vec![
            TextContentOrImage::text("other"),
            TextContentOrImage::text(text)
        ]
    );
    assert_eq!(result.details, json!({"externalToolResult":raw}));
}

#[test]
fn unequal_or_non_json_text_does_not_hide_structured_result() {
    for text in [r#"{"x":1}"#, "not JSON", "```json\n{\"x\":2}\n```"] {
        let result = decode_result(json!({
            "content":[{"type":"text","text":text}],
            "structuredContent":{"x":2}
        }))
        .unwrap();
        assert_eq!(
            result.content,
            vec![
                TextContentOrImage::text(text),
                TextContentOrImage::text(r#"{"x":2}"#)
            ]
        );
    }
}

#[test]
fn structured_content_is_optional_but_must_be_an_object_when_present() {
    let plain = decode_result(json!({"content":[]})).unwrap();
    assert!(plain.content.is_empty());
    let empty = decode_result(json!({"content":[],"structuredContent":{}})).unwrap();
    assert_eq!(empty.content, vec![TextContentOrImage::text("{}")]);
    for invalid in [
        json!([]),
        json!([{}]),
        json!("text"),
        json!(42),
        json!(true),
        Value::Null,
    ] {
        let raw = json!({"content":[],"structuredContent":invalid});
        assert!(matches!(decode_result(raw), Err(AgentError::Validation(_))));
    }
}

#[test]
fn error_keeps_complete_raw_content_and_structured_result() {
    let raw = json!({
        "content":[{"type":"text","text":"backend failure"}],
        "structuredContent":{"reason":"failed","retry":false},
        "_meta":{"trace":"original"},
        "isError":true
    });
    let Err(AgentError::Tool(error)) = decode_result(raw.clone()) else {
        panic!("isError must remain a tool error");
    };
    let projected: Value = serde_json::from_str(&error).unwrap();
    assert_eq!(projected["content"], raw["content"]);
    assert_eq!(projected["structuredContent"], raw["structuredContent"]);
    assert_eq!(projected["isError"], true);
    assert!(projected.get("_meta").is_none());
}

#[test]
fn structured_projection_does_not_truncate_large_utf8_values() {
    let text = "雪".repeat(100_000);
    let raw = json!({"content":[],"structuredContent":{"text":text}});
    let result = decode_result(raw.clone()).unwrap();
    let TextContentOrImage::Text(projected) = &result.content[0] else {
        panic!("expected text");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&projected.text).unwrap(),
        raw["structuredContent"]
    );
    assert_eq!(result.details["externalToolResult"], raw);
}
