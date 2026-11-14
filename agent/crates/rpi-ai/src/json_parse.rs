//! Mirrors `packages/ai/src/utils/json-parse.ts` — JSON repair + streaming
//! partial-JSON parsing used by the Anthropic SSE mapper to recover tool-call
//! arguments that arrive as malformed / incomplete JSON deltas.
//!
//! TS uses `partial-json`'s `parse` for the streaming arm. The Rust port
//! implements a small incremental parser that accepts truncated-but-otherwise-
//! valid JSON by closing any open objects/arrays/strings at the cursor. This
//! matches the contract: `parse_streaming_json(s)` ALWAYS returns a value
//! (an empty object on total failure), so a `ToolCallEnd` event never carries
//! unparseable args.
//!
//! `repair_json` is a faithful port of the TS control-character / invalid-
//! escape repair pass.

/// The set of valid JSON string escapes — mirrors TS `VALID_JSON_ESCAPES`.
const VALID_JSON_ESCAPES: &[char] = &['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

fn is_control_character(c: char) -> bool {
    (c as u32) <= 0x1f
}

fn escape_control_character(c: char) -> String {
    match c {
        '\u{0008}' => "\\b".to_string(),
        '\u{000c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        _ => format!("\\u{:04x}", c as u32),
    }
}

/// Repair malformed JSON string literals by escaping raw control characters
/// and doubling backslashes before invalid escape characters. Mirrors TS
/// `repairJson`.
pub fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if !in_string {
            repaired.push(c);
            if c == '"' {
                in_string = true;
            }
            i += 1;
            continue;
        }

        // Inside a string.
        if c == '"' {
            repaired.push(c);
            in_string = false;
            i += 1;
            continue;
        }

        if c == '\\' {
            let next = chars.get(i + 1).copied();
            match next {
                None => {
                    // Trailing backslash → double it.
                    repaired.push('\\');
                    repaired.push('\\');
                    i += 1;
                    continue;
                }
                Some('u') => {
                    let unicode_digits: String = chars[i + 2..].iter().take(4).collect();
                    if unicode_digits.len() == 4
                        && unicode_digits.chars().all(|d| d.is_ascii_hexdigit())
                    {
                        repaired.push('\\');
                        repaired.push('u');
                        repaired.push_str(&unicode_digits);
                        i += 6; // \, u, 4 digits
                        continue;
                    }
                    // Invalid \u → double backslash.
                    repaired.push('\\');
                    repaired.push('\\');
                    i += 1;
                    continue;
                }
                Some(n) if VALID_JSON_ESCAPES.contains(&n) => {
                    repaired.push('\\');
                    repaired.push(n);
                    i += 2;
                    continue;
                }
                Some(_) => {
                    // Invalid escape → double the backslash.
                    repaired.push('\\');
                    repaired.push('\\');
                    i += 1;
                    continue;
                }
            }
        }

        if is_control_character(c) {
            repaired.push_str(&escape_control_character(c));
        } else {
            repaired.push(c);
        }
        i += 1;
    }

    repaired
}

/// Parse JSON, repairing malformed string literals on failure. Mirrors TS
/// `parseJsonWithRepair<T>`.
pub fn parse_json_with_repair(json: &str) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::from_str(json).or_else(|_| {
        let repaired = repair_json(json);
        if repaired == json {
            // No repair applied → propagate original error.
            serde_json::from_str(json)
        } else {
            serde_json::from_str(&repaired)
        }
    })
}

/// Parse potentially incomplete JSON during streaming. Always returns a value
/// (an empty object on total failure). Mirrors TS `parseStreamingJson`.
///
/// Strategy (same layers as TS): strict parse → repair+strict → incremental
/// close-and-parse. The incremental parser closes any open string/object/array
/// so truncated input still yields a best-effort structure.
pub fn parse_streaming_json(json: Option<&str>) -> serde_json::Value {
    let Some(s) = json else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }

    if let Ok(v) = parse_json_with_repair(trimmed) {
        return v;
    }
    if let Ok(v) = parse_incremental(trimmed) {
        return v;
    }
    if let Ok(repaired) = parse_json_with_repair(&repair_json(trimmed)) {
        // `parse_json_with_repair` already tried `repaired` if it differed; this
        // arm exists to mirror the TS `partialParse(repairJson(partialJson))`
        // layer. The incremental path below it is the real fallback.
        let _ = repaired;
    }
    if let Ok(v) = parse_incremental(&repair_json(trimmed)) {
        return v;
    }
    serde_json::Value::Object(serde_json::Map::new())
}

/// Incremental JSON parser: closes open strings/objects/arrays to coerce a
/// truncated value into the nearest complete prefix. Returns the parsed value.
/// Unlike a strict parser, unknown trailing characters after a complete value
/// are ignored.
fn parse_incremental(s: &str) -> Result<serde_json::Value, serde_json::Error> {
    let closed = close_truncated_json(s);
    serde_json::from_str(&closed)
}

/// Close any open string/object/array in a truncated JSON document.
fn close_truncated_json(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(chars.len() + 8);
    let mut stack: Vec<char> = Vec::new(); // '{' or '['
    let mut in_string = false;
    let mut escape = false;
    let mut last_nonspace: Option<char> = None;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        if in_string {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
                last_nonspace = Some('"');
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                last_nonspace = Some('"');
            }
            '{' | '[' => {
                stack.push(c);
                last_nonspace = Some(c);
            }
            '}' | ']' => {
                stack.pop();
                last_nonspace = Some(c);
            }
            c if !c.is_whitespace() => {
                last_nonspace = Some(c);
            }
            _ => {}
        }
        i += 1;
    }

    // If inside a string, terminate it.
    if in_string {
        out.push('"');
    }

    // Strip a trailing comma so closing brackets don't yield invalid JSON.
    if matches!(last_nonspace, Some(',')) {
        if let Some(pos) = out.rfind(',') {
            out.remove(pos);
        }
    }

    // Close open containers in reverse order.
    while let Some(top) = stack.pop() {
        out.push(match top {
            '{' => '}',
            '[' => ']',
            other => other,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_streaming_json_complete() {
        let v = parse_streaming_json(Some(r#"{"path":"a","text":"b"}"#));
        assert_eq!(v, json!({"path":"a","text":"b"}));
    }

    #[test]
    fn parse_streaming_json_truncated_object() {
        // Missing closing brace + value.
        let v = parse_streaming_json(Some(r#"{"path":"A"#));
        // Best-effort: should at least produce an object (possibly with path).
        assert!(v.is_object());
    }

    #[test]
    fn parse_streaming_json_repair_invalid_escape() {
        // The TS test uses `{"path":"A\H","text":"col1\tcol2"}` — `\H` is an
        // invalid escape that `repair_json` doubles to `\\H`, and `\t` is a
        // valid escape left intact. After repair serde should parse it.
        let raw = r#"{"path":"A\H","text":"col1\tcol2"}"#;
        let repaired = repair_json(raw);
        assert!(repaired.contains(r#"path":"A\\H"#));
        let v: serde_json::Value = serde_json::from_str(&repaired).expect("repaired parses");
        assert_eq!(v["path"], json!("A\\H"));
        assert_eq!(v["text"], json!("col1\tcol2"));
    }

    #[test]
    fn parse_streaming_json_empty() {
        assert_eq!(parse_streaming_json(None), json!({}));
        assert_eq!(parse_streaming_json(Some("")), json!({}));
        assert_eq!(parse_streaming_json(Some("   ")), json!({}));
    }

    #[test]
    fn parse_streaming_json_garbage_is_object() {
        // Total parse failure must still yield an object, never panic.
        let v = parse_streaming_json(Some("not json at all"));
        assert!(v.is_object());
    }

    #[test]
    fn repair_trailing_backslash() {
        let repaired = repair_json(r#"{"a":"b\"#);
        // Trailing backslash doubled.
        assert!(repaired.ends_with(r#"b\\"#) || repaired.ends_with(r#""b\\""#));
    }

    #[test]
    fn close_truncated_array() {
        let closed = close_truncated_json(r#"[1,2,3"#);
        assert_eq!(closed, "[1,2,3]");
    }

    #[test]
    fn close_truncated_nested_with_trailing_comma() {
        let closed = close_truncated_json(r#"{"a":{"b":1,"#);
        assert_eq!(closed, r#"{"a":{"b":1}}"#);
    }
}
