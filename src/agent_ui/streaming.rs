//! Recover complete nodes from an unfinished UI JSON object.
//!
//! This is only a preview: the closed fence is always parsed and validated in
//! full. Strings and partial nodes are never repaired or interpreted as code.

use super::{MAX_SPEC_BYTES, UiSpec, parse_spec};

pub(super) fn partial_spec(raw: &str) -> Option<UiSpec> {
    if raw.len() > MAX_SPEC_BYTES {
        return None;
    }
    if let Ok(spec) = parse_spec(raw) {
        return Some(spec);
    }

    // A complete node ends at object depth three: root, elements, node.
    // Remember only the latest such boundary, ignoring braces inside strings.
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut boundary = None;
    for (index, byte) in raw.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => stack.push(byte),
            b'}' | b']' => {
                let expected = if byte == b'}' { b'{' } else { b'[' };
                if stack.pop() != Some(expected) {
                    break;
                }
                if byte == b'}' && stack.as_slice() == [b'{', b'{'] {
                    boundary = Some(index + 1);
                }
            }
            _ => {}
        }
    }
    let end = boundary?;
    let mut candidate = String::with_capacity(end + 2);
    candidate.push_str(&raw[..end]);
    candidate.push_str("}}");
    let mut spec: UiSpec = serde_json::from_str(&candidate).ok()?;
    // A parent can arrive before its children. Keep its completed content and
    // attach each child as soon as that child's full node has arrived.
    let available: std::collections::HashSet<_> = spec.elements.keys().cloned().collect();
    for node in spec.elements.values_mut() {
        node.children.retain(|id| available.contains(id));
    }
    let candidate = serde_json::to_string(&spec).ok()?;
    parse_spec(&candidate).ok()
}
