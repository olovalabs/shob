//! Shared YAML-frontmatter parser for the skills and prompt-template loaders.
//!
//! The TS duplicates `parseFrontmatter` verbatim in both `skills.ts` and
//! `prompt-templates.ts`; consolidated here to avoid the duplication. The TS
//! parses the fenced YAML block with the `yaml` npm package. This port uses
//! `yaml_serde`, including standard block sequences, nested mappings, and
//! block scalars. Malformed frontmatter produces a `parse_failed` diagnostic
//! just like the TypeScript implementation.

use serde_json::{Map, Value};

/// Parse a `---\n...\n---` frontmatter block. Returns `(frontmatter_object,
/// body)` on success, or `Err(message)` when the fenced YAML is malformed.
///
/// Mirrors TS `parseFrontmatter`:
/// - CRLF/CR normalized to LF.
/// - No leading `---` → empty frontmatter, body = the whole normalized content
///   (NOT trimmed — matches TS).
/// - No closing `\n---` → same as no-frontmatter.
/// - Otherwise yaml = between the fences; body = after the closing fence,
///   trimmed.
pub(crate) fn parse_frontmatter(content: &str) -> Result<(Value, String), String> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Ok((Value::Object(Map::new()), normalized));
    }
    // Search for the closing `\n---` after the opening fence.
    let Some(end_rel) = normalized[3..].find("\n---") else {
        return Ok((Value::Object(Map::new()), normalized));
    };
    let end_index = end_rel + 3; // index of the `\n` in `\n---`
                                 // `slice(4, endIndex)` in TS; empty when endIndex < 4.
    let yaml_string = normalized.get(4..end_index).unwrap_or("");
    // `slice(endIndex + 4)` skips `\n---`; `.trim()`.
    let body = normalized
        .get(end_index + 4..)
        .unwrap_or("")
        .trim()
        .to_string();
    let frontmatter = parse_yaml(yaml_string)?;
    Ok((frontmatter, body))
}

fn parse_yaml(input: &str) -> Result<Value, String> {
    if input.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match yaml_serde::from_str(input) {
        Ok(value) => Ok(value),
        Err(original_error) => {
            // Older rpi releases treated every top-level value as an opaque
            // scalar, so descriptions containing an unquoted `: ` were
            // accepted even though strict YAML requires quoting them. Retry
            // only after quoting that narrow legacy shape; block collections
            // and all other syntax remain standard YAML.
            let compatible = quote_legacy_top_level_scalars(input);
            if compatible == input {
                return Err(original_error.to_string());
            }
            yaml_serde::from_str(&compatible).map_err(|_| original_error.to_string())
        }
    }
}

fn quote_legacy_top_level_scalars(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            if line.starts_with(char::is_whitespace) || line.trim_start().starts_with('#') {
                return line.to_string();
            }
            let Some((key, raw_value)) = line.split_once(':') else {
                return line.to_string();
            };
            let value = raw_value.trim();
            let collection_or_quoted = value.starts_with(['"', '\'', '[', '{', '|', '>']);
            if value.contains(": ") && !collection_or_quoted {
                format!(
                    "{}: {}",
                    key.trim_end(),
                    serde_json::to_string(value).expect("string serialization cannot fail")
                )
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_returns_whole_body_untrimmed() {
        let (fm, body) = parse_frontmatter("First line\nBody\n").unwrap();
        assert!(fm.as_object().unwrap().is_empty());
        assert_eq!(body, "First line\nBody\n");
    }

    #[test]
    fn parses_string_and_bool_fields() {
        let content = "---\nname: example\ndescription: Example skill\ndisable-model-invocation: true\n---\nUse this skill.\n";
        let (fm, body) = parse_frontmatter(content).unwrap();
        let o = fm.as_object().unwrap();
        assert_eq!(o.get("name").and_then(|v| v.as_str()), Some("example"));
        assert_eq!(
            o.get("description").and_then(|v| v.as_str()),
            Some("Example skill")
        );
        assert_eq!(
            o.get("disable-model-invocation").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(body, "Use this skill.");
    }

    #[test]
    fn empty_value_is_null() {
        let (fm, _) = parse_frontmatter("---\ndescription:\n---\nbody").unwrap();
        assert!(fm.get("description").unwrap().is_null());
    }

    #[test]
    fn unterminated_flow_sequence_is_parse_error() {
        let content = "---\ndescription: [unterminated\n---\nBody";
        assert!(parse_frontmatter(content).is_err());
    }

    #[test]
    fn flow_array_parses() {
        let (fm, _) = parse_frontmatter("---\ntags: [a, b, c]\n---\nx").unwrap();
        let arr = fm.get("tags").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0].as_str(), Some("a"));
    }

    #[test]
    fn block_sequence_parses() {
        let content = "---\nname: flowchart\ndescription: Draw diagrams\ntriggers:\n  - \"流程图\"\n  - \"原理演示\"\n---\nUse this skill.";
        let (fm, body) = parse_frontmatter(content).unwrap();
        let triggers = fm.get("triggers").unwrap().as_array().unwrap();

        assert_eq!(triggers.len(), 2);
        assert_eq!(triggers[0].as_str(), Some("流程图"));
        assert_eq!(triggers[1].as_str(), Some("原理演示"));
        assert_eq!(body, "Use this skill.");
    }

    #[test]
    fn nested_mapping_and_block_scalar_parse() {
        let content = "---\nmetadata:\n  owner: animation\n  enabled: true\ndescription: |\n  First line\n  Second line\n---\nBody";
        let (fm, _) = parse_frontmatter(content).unwrap();

        assert_eq!(fm["metadata"]["owner"], "animation");
        assert_eq!(fm["metadata"]["enabled"], true);
        assert_eq!(fm["description"], "First line\nSecond line");
    }

    #[test]
    fn legacy_unquoted_colon_in_top_level_description_remains_compatible() {
        let content =
            "---\nname: art\ndescription: Animation pipeline: sprites, video, and FX.\n---\nBody";
        let (fm, _) = parse_frontmatter(content).unwrap();

        assert_eq!(
            fm["description"],
            "Animation pipeline: sprites, video, and FX."
        );
    }

    #[test]
    fn missing_closing_fence_treated_as_no_frontmatter() {
        let (fm, body) = parse_frontmatter("---\nname: x\nbody still here").unwrap();
        assert!(fm.as_object().unwrap().is_empty());
        assert!(body.contains("name: x"));
    }
}
