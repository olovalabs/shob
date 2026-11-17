use super::*;
use crate::session::{
    jsonl::codec::parse_mutation,
    types::{LaneRecord, SessionMutation},
};
use serde_json::{json, Value};

fn row() -> Value {
    json!({
        "kind": "record",
        "type": "assistant_frame",
        "id": "frame-1",
        "seq": 1,
        "lane": "main",
        "timestamp": 42,
        "runId": "run-1",
        "streamIndex": 0,
        "op": "append",
        "frame": {"kind": "delta", "text": "hello"}
    })
}

/// A fast-path success must be equivalent to the reference decoder, including
/// every piece of metadata exposed to indexing. None always leaves authority
/// with the reference, whether it accepts or rejects the original row.
fn differential(line: &str, expect_fast: bool) {
    let fast = parse_cold_frame(line);
    let slow = parse_mutation(line);
    assert_eq!(fast.is_some(), expect_fast, "fast-path decision: {line}");
    if let Some(fast) = fast {
        let SessionMutation::Record {
            record: LaneRecord::AssistantFrame(slow),
        } = slow.unwrap_or_else(|error| panic!("fast accepted invalid row: {error}: {line}"))
        else {
            panic!("fast accepted a different mutation: {line}");
        };
        assert_eq!(fast.id, slow.base.id);
        assert_eq!(fast.seq, slow.base.seq);
        assert_eq!(fast.lane, slow.base.lane);
        assert_eq!(fast.timestamp, slow.base.timestamp);
        assert_eq!(fast.run_id, slow.run_id);
        assert_eq!(fast.stream_index, slow.stream_index);
        assert_eq!(fast.op, slow.op);
        assert_ne!(fast.op, AssistantFrameOp::CommitStream);
    }
}

fn numeric(field: &str, token: &str) -> String {
    let mut value = row();
    value[field] = Value::String("__numeric_token__".into());
    serde_json::to_string(&value)
        .unwrap()
        .replace("\"__numeric_token__\"", token)
}

#[test]
fn operations_and_arbitrary_optional_frames_match_reference() {
    for (op, expected) in [
        ("append", true),
        ("clear_stream", true),
        ("clear_run", true),
        ("commit_stream", false),
    ] {
        for frame in [
            None,
            Some(Value::Null),
            Some(json!({})),
            Some(json!([])),
            Some(json!("")),
            Some(json!("text\n中文🚀")),
            Some(json!(true)),
            Some(json!(false)),
            Some(json!(-123)),
            Some(json!(1.25)),
            Some(json!([null, true, {"nested": ["value", 7]}])),
            Some(json!({"entryId": "reserved-entry"})),
        ] {
            let mut value = row();
            value["op"] = json!(op);
            match frame {
                Some(frame) => value["frame"] = frame,
                None => {
                    value.as_object_mut().unwrap().remove("frame");
                }
            }
            let line = value.to_string();
            assert!(parse_mutation(&line).is_ok(), "{line}");
            differential(&line, expected);
        }
    }
}

#[test]
fn required_fields_and_types_match_reference() {
    for field in [
        "kind",
        "type",
        "id",
        "seq",
        "lane",
        "timestamp",
        "runId",
        "streamIndex",
        "op",
    ] {
        let mut value = row();
        value.as_object_mut().unwrap().remove(field);
        let line = value.to_string();
        assert!(parse_mutation(&line).is_err(), "missing {field}");
        differential(&line, false);

        for invalid in [Value::Null, json!([]), json!({}), json!(true)] {
            let mut value = row();
            value[field] = invalid;
            let line = value.to_string();
            assert!(parse_mutation(&line).is_err(), "invalid {field}: {line}");
            differential(&line, false);
        }
    }
    for field in ["id", "lane", "runId"] {
        let mut value = row();
        value[field] = json!("");
        differential(&value.to_string(), true);
        value[field] = json!(123);
        differential(&value.to_string(), false);
    }
    for (field, text) in [
        ("kind", "entry"),
        ("type", "usage"),
        ("op", "unknown"),
        ("op", "Append"),
        ("seq", "1"),
        ("timestamp", "42"),
        ("streamIndex", "0"),
    ] {
        let mut value = row();
        value[field] = json!(text);
        differential(&value.to_string(), false);
    }
    for line in ["null", "[]", "true", "1", "\"record\""] {
        differential(line, false);
    }
}

#[test]
fn sequence_uses_positive_u64_not_javascript_safe_integer_limit() {
    for (token, accepted) in [
        ("0", false),
        ("-0", false),
        ("-1", false),
        ("1", true),
        ("9007199254740991", true),
        ("9007199254740992", true),
        ("9223372036854775807", true),
        ("9223372036854775808", true),
        ("18446744073709551615", true),
        ("18446744073709551616", false),
        ("1.0", false),
        ("1e0", false),
        ("1e1", false),
        ("1e309", false),
        ("1e-400", false),
        ("+1", false),
        ("01", false),
    ] {
        let line = numeric("seq", token);
        assert_eq!(parse_mutation(&line).is_ok(), accepted, "{token}");
        differential(&line, accepted);
    }
}

#[test]
fn timestamp_uses_signed_record_base_semantics() {
    for (token, accepted) in [
        ("-9223372036854775809", false),
        ("-9223372036854775808", true),
        ("-42", true),
        ("-0", false),
        ("0", true),
        ("9223372036854775807", true),
        ("9223372036854775808", false),
        ("18446744073709551615", false),
        ("42.0", false),
        ("4.2e1", false),
        ("-1e309", false),
    ] {
        let line = numeric("timestamp", token);
        assert_eq!(parse_mutation(&line).is_ok(), accepted, "{token}");
        differential(&line, accepted);
    }
}

#[test]
fn stream_index_uses_platform_usize_range() {
    for (token, accepted) in [
        ("-1".to_owned(), false),
        ("-0".to_owned(), false),
        ("0".to_owned(), true),
        ("1".to_owned(), true),
        (usize::MAX.to_string(), true),
        ((usize::MAX as u128 + 1).to_string(), false),
        ("18446744073709551616".to_owned(), false),
        ("0.0".to_owned(), false),
        ("0e0".to_owned(), false),
    ] {
        let line = numeric("streamIndex", &token);
        assert_eq!(parse_mutation(&line).is_ok(), accepted, "{token}");
        differential(&line, accepted);
    }
}

#[test]
fn borrowed_strings_and_escaped_keys_and_values() {
    let line = r#"{"kind":"record","type":"assistant_frame","id":"id","seq":1,"lane":"lane","timestamp":-1,"runId":"run","streamIndex":0,"op":"append"}"#;
    let frame = parse_cold_frame(line).unwrap();
    assert!(matches!(frame.id, Cow::Borrowed("id")));
    assert!(matches!(frame.lane, Cow::Borrowed("lane")));
    assert!(matches!(frame.run_id, Cow::Borrowed("run")));
    differential(line, true);

    let line = r#"{"k\u0069nd":"rec\u006frd","ty\u0070e":"assistant_frame","i\u0064":"i\nd","seq":1,"lane":"la\tne","timestamp":-1,"run\u0049d":"r\uD83D\uDE80","streamIndex":0,"op":"app\u0065nd","fr\u0061me":{"\uD83D\uDE80":"\u0000"}}"#;
    let frame = parse_cold_frame(line).unwrap();
    assert!(matches!(frame.id, Cow::Owned(_)));
    assert!(matches!(frame.lane, Cow::Owned(_)));
    assert!(matches!(frame.run_id, Cow::Owned(_)));
    assert_eq!(frame.run_id, "r🚀");
    differential(line, true);
}

#[test]
fn known_duplicates_always_fall_back_to_last_wins_codec() {
    for field in [
        "kind",
        "type",
        "id",
        "seq",
        "lane",
        "timestamp",
        "runId",
        "streamIndex",
        "op",
        "frame",
    ] {
        let value = row();
        let extra = format!("{field:?}:{}", value[field]);
        let original = value.to_string();
        let line = format!("{{{extra},{}", &original[1..]);
        assert!(parse_mutation(&line).is_ok(), "{line}");
        differential(&line, false);
    }
    let original = row().to_string();
    for prefix in [
        r#""id":null"#,
        r#""seq":-1"#,
        r#""op":"commit_stream""#,
        r#""kind":"entry""#,
        r#""i\u0064":"earlier""#,
    ] {
        let line = format!("{{{prefix},{}", &original[1..]);
        assert!(parse_mutation(&line).is_ok(), "{line}");
        differential(&line, false);
    }
}

#[test]
fn arbitrary_order_unknown_fields_and_nested_duplicates_are_validated() {
    let value = row();
    let mut fields: Vec<_> = value
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| format!("{key:?}:{value}"))
        .collect();
    for rotation in 0..fields.len() {
        fields.rotate_left(1);
        let line = format!(
            "{{\"unknown\":{{\"same\":1,\"same\":2}}, {}, \"unknown\":[null,true,{{\"x\":\"value\"}}]}}",
            fields.join(",")
        );
        differential(&line, true);
        if rotation % 2 == 0 {
            fields.reverse();
        }
    }
}

#[test]
fn discarded_numbers_are_checked_exactly_like_value() {
    for field in ["frame", "unknown"] {
        for (token, accepted) in [
            ("0", true),
            ("-0", true),
            ("1.0", true),
            ("1e308", true),
            ("1e309", false),
            ("-1e309", false),
            ("1e-400", true),
            ("18446744073709551616", true),
            ("-9223372036854775809", true),
            ("01", false),
            ("NaN", false),
            ("Infinity", false),
        ] {
            for payload in [token.to_owned(), format!("[{{\"number\":{token}}}]")] {
                let line = numeric(field, &payload);
                assert_eq!(parse_mutation(&line).is_ok(), accepted, "{line}");
                differential(&line, accepted);
            }
        }
    }
}

#[test]
fn invalid_strings_keys_and_json_never_succeed() {
    for field in ["frame", "unknown"] {
        for invalid in [
            r#""\q""#,
            r#""\uD800""#,
            r#""\uDC00""#,
            r#""\uD800\u0041""#,
            r#""\uZZZZ""#,
            r#"{"\uD800":1}"#,
            r#"{"\q":1}"#,
            r#"[1,]"#,
            r#"{"key":true,}"#,
            "\"raw\nnewline\"",
            "\"raw\u{0}nul\"",
            "[null",
            "{\"key\":",
        ] {
            let line = numeric(field, invalid);
            assert!(parse_mutation(&line).is_err(), "{line}");
            differential(&line, false);
        }
    }
    let line = row().to_string();
    for tail in ["x", " null", "{}", ","] {
        differential(&format!("{line}{tail}"), false);
    }
    for end in 0..line.len() {
        // This synthetic row is ASCII, so every truncation is a char boundary.
        differential(&line[..end], false);
    }
    differential(&format!(" \t\r\n{line}\n\t "), true);
}

#[test]
fn discarded_recursion_has_the_reference_depth_limit() {
    for field in ["frame", "unknown"] {
        for depth in 0..=140 {
            for (open, close) in [("[", "]"), ("{\"nested\":", "}")] {
                let payload = format!("{}null{}", open.repeat(depth), close.repeat(depth));
                let line = numeric(field, &payload);
                let accepted = parse_mutation(&line).is_ok();
                differential(&line, accepted);
                if depth == 140 {
                    assert!(!accepted, "default JSON recursion limit must stay enabled");
                }
            }
        }
    }
}

fn raw_value_entry(escaped: bool, value: &str) -> String {
    let key = if escaped {
        r#""$serde_json::private::RawVal\u0075e""#
    } else {
        r#""$serde_json::private::RawValue""#
    };
    format!("{key}:{value}")
}

#[test]
fn raw_value_keys_always_leave_the_fast_path_to_value() {
    // Value treats this decoded key as a protocol marker, not as an ordinary
    // map key. Exercise every discard shape and both source spellings; the
    // canonical result is intentionally allowed to be either accepted or
    // rejected, but the fast path must never decide it itself.
    for value in [
        "0",               // non-string raw value
        "null",            // non-string raw value
        r#""not json""#,   // invalid JSON in a raw string
        r#""1e309""#,      // non-finite JSON in a raw string
        r#""{\"ok\":1}""#, // valid raw JSON string
    ] {
        for escaped in [false, true] {
            let entry = raw_value_entry(escaped, value);
            let frame = row().to_string().replacen(
                r#"{"kind":"delta","text":"hello"}"#,
                &format!("{{{entry}}}"),
                1,
            );
            let unknown = format!("{{\"unknown\":{{{entry}}},{}", &row().to_string()[1..]);
            let array_nested = row().to_string().replacen(
                r#"{"kind":"delta","text":"hello"}"#,
                &format!("{{\"outer\":[{{{entry}}}]}}"),
                1,
            );
            let root = format!("{{{entry},{}", &row().to_string()[1..]);
            let root_nonfirst = format!("{{\"ordinary\":0,{entry},{}", &row().to_string()[1..]);
            for line in [frame, unknown, array_nested, root, root_nonfirst] {
                let _canonical = parse_mutation(&line);
                assert!(
                    parse_cold_frame(&line).is_none(),
                    "raw_value key must force canonical fallback: {line}"
                );
            }
        }
    }

    // Neither a non-first occurrence nor a duplicate gets mistaken for an
    // ordinary discarded key. This also covers duplicate decoded spellings.
    for object in [
        r#"{"$serde_json::private::RawValue":0,"$serde_json::private::RawValue":"{}"}"#,
        r#"{"$serde_json::private::RawValue":0,"$serde_json::private::RawVal\u0075e":"{}"}"#,
    ] {
        let line = row()
            .to_string()
            .replacen(r#"{"kind":"delta","text":"hello"}"#, object, 1);
        let _canonical = parse_mutation(&line);
        assert!(parse_cold_frame(&line).is_none(), "{line}");
    }
}

#[test]
fn ordinary_kind_hint_never_replaces_complete_codec_validation() {
    for kind in ["entry", "lane", "fact"] {
        let line = row()
            .to_string()
            .replacen("{", &format!("{{\"kind\":\"{kind}\","), 1);
        // A duplicate discriminator is resolved by the complete last-wins
        // codec; rejecting the optimization must not reject a valid record.
        assert!(parse_mutation(&line).is_ok());
        differential(&line, false);
        assert!(parse_mutation(&format!("{line} broken")).is_err());
        differential(&format!("{line} broken"), false);
    }
}

#[test]
fn uncommon_enum_maps_are_conservatively_left_to_reference() {
    let mut value = row();
    for op in ["append", "clear_stream", "clear_run", "commit_stream"] {
        value["op"] = Value::Object([(op.to_owned(), Value::Null)].into_iter().collect());
        // These representations are deliberately outside the recognizer even
        // if serde's enum decoder accepts them. The caller runs the old codec.
        differential(&value.to_string(), false);
    }
}
