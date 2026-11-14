//! Constrained sampling — grammar-constrained tool output formatting.
//!
//! Mirrors `packages/ai/src/api/constrained-sampling.ts`. Provides support for
//! grammar-constrained sampling (Lark/regex) to enforce structured output formats
//! on tool calls.
//!
//! # Overview
//!
//! Some models support grammar-constrained generation, where the output is forced
//! to match a specific grammar (e.g., Lark or regex). This is useful for:
//!
//! - Ensuring tool arguments match a specific schema
//! - Generating valid JSON/XML/YAML
//! - Enforcing custom formats (e.g., phone numbers, dates)
//!
//! # Support Matrix
//!
//! | Provider | Grammar Format | Notes |
//! |----------|---------------|-------|
//! | Anthropic | JSON Schema (strict mode) | Via `strict: true` in tool definition |
//! | OpenAI | JSON Schema (strict mode) | Via `strict: true` in tool definition |
//! | Ollama | Lark / Regex | Via `format` parameter |
//!
//! # Example
//!
//! ```rust
//! use rpi_ai::constrained::{GrammarConstraint, GrammarFormat};
//!
//! // Define a constraint: output must match this SSN-shaped regex.
//! let constraint = GrammarConstraint::regex(r"^\d{3}-\d{2}-\d{4}$")
//!     .with_input_property("value");
//!
//! assert_eq!(constraint.format, GrammarFormat::Regex);
//! assert_eq!(constraint.input_property.as_deref(), Some("value"));
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Grammar format for constrained sampling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrammarFormat {
    /// Lark grammar format (EBNF-like)
    Lark,
    /// Regular expression format
    Regex,
    /// JSON Schema (for strict mode)
    JsonSchema,
}

/// A grammar constraint for tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrammarConstraint {
    /// The grammar format (Lark, Regex, or JsonSchema).
    pub format: GrammarFormat,
    /// The grammar definition (Lark grammar, regex pattern, or JSON Schema).
    pub definition: String,
    /// Optional: which input property this constraint applies to.
    /// If `None`, applies to the entire output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_property: Option<String>,
}

impl GrammarConstraint {
    /// Create a regex constraint.
    pub fn regex(pattern: impl Into<String>) -> Self {
        Self {
            format: GrammarFormat::Regex,
            definition: pattern.into(),
            input_property: None,
        }
    }

    /// Create a Lark grammar constraint.
    pub fn lark(grammar: impl Into<String>) -> Self {
        Self {
            format: GrammarFormat::Lark,
            definition: grammar.into(),
            input_property: None,
        }
    }

    /// Create a JSON Schema constraint.
    pub fn json_schema(schema: Value) -> Self {
        Self {
            format: GrammarFormat::JsonSchema,
            definition: serde_json::to_string(&schema).unwrap_or_default(),
            input_property: None,
        }
    }

    /// Set the input property this constraint applies to.
    pub fn with_input_property(mut self, property: impl Into<String>) -> Self {
        self.input_property = Some(property.into());
        self
    }
}

/// Trait for providers that support grammar-constrained sampling.
pub trait ConstrainedSamplingSupport {
    /// Check if this provider supports the given grammar format.
    fn supports_grammar_format(&self, format: &GrammarFormat) -> bool;

    /// Convert a grammar constraint to provider-specific parameters.
    /// Returns `None` if the constraint is not supported.
    fn constraint_to_params(&self, constraint: &GrammarConstraint) -> Option<Value>;
}

/// Validate a regex pattern.
///
/// # Arguments
///
/// * `pattern` - The regex pattern to validate
///
/// # Returns
///
/// * `Ok(())` - Pattern is valid
/// * `Err(String)` - Pattern is invalid
pub fn validate_regex(pattern: &str) -> Result<(), String> {
    // Basic validation: check for balanced brackets/parens
    let mut bracket_depth = 0;
    let mut paren_depth = 0;
    let mut escape_next = false;

    for ch in pattern.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match ch {
            '\\' => escape_next = true,
            '[' => bracket_depth += 1,
            ']' => {
                if bracket_depth == 0 {
                    return Err("Unbalanced ] in regex".to_string());
                }
                bracket_depth -= 1;
            }
            '(' => paren_depth += 1,
            ')' => {
                if paren_depth == 0 {
                    return Err("Unbalanced ) in regex".to_string());
                }
                paren_depth -= 1;
            }
            _ => {}
        }
    }

    if escape_next {
        return Err("Trailing backslash in regex".to_string());
    }

    if bracket_depth != 0 {
        return Err("Unbalanced [ in regex".to_string());
    }

    if paren_depth != 0 {
        return Err("Unbalanced ( in regex".to_string());
    }

    Ok(())
}

/// Validate a Lark grammar (basic syntax check).
///
/// # Arguments
///
/// * `grammar` - The Lark grammar to validate
///
/// # Returns
///
/// * `Ok(())` - Grammar appears valid
/// * `Err(String)` - Grammar has syntax errors
///
/// # Note
///
/// This is a basic syntax check. Full validation requires a Lark parser.
pub fn validate_lark_grammar(grammar: &str) -> Result<(), String> {
    // Basic checks
    if grammar.trim().is_empty() {
        return Err("Grammar is empty".to_string());
    }

    // Check for balanced braces
    let mut brace_count = 0;
    for ch in grammar.chars() {
        match ch {
            '{' => brace_count += 1,
            '}' => {
                brace_count -= 1;
                if brace_count < 0 {
                    return Err("Unbalanced braces in grammar".to_string());
                }
            }
            _ => {}
        }
    }

    if brace_count != 0 {
        return Err("Unbalanced braces in grammar".to_string());
    }

    // Check for at least one rule definition (contains ":")
    if !grammar.contains(':') {
        return Err("Grammar must contain at least one rule (e.g., 'start: ...')".to_string());
    }

    Ok(())
}

/// Validate a JSON Schema.
///
/// # Arguments
///
/// * `schema` - The JSON Schema to validate
///
/// # Returns
///
/// * `Ok(())` - Schema is valid
/// * `Err(String)` - Schema is invalid
pub fn validate_json_schema(schema: &Value) -> Result<(), String> {
    // Basic checks
    if !schema.is_object() {
        return Err("JSON Schema must be an object".to_string());
    }

    let obj = schema.as_object().unwrap();

    // Must have a "type" field
    if !obj.contains_key("type") {
        return Err("JSON Schema must have a 'type' field".to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_regex_valid() {
        assert!(validate_regex(r"^\d{3}-\d{2}-\d{4}$").is_ok());
        assert!(validate_regex(r"[a-z]+").is_ok());
        assert!(validate_regex(r"").is_ok()); // Empty regex is valid
    }

    #[test]
    fn test_validate_regex_invalid() {
        assert!(validate_regex(r"[unclosed").is_err());
        assert!(validate_regex(r"(?P<invalid").is_err());
    }

    #[test]
    fn test_validate_lark_grammar_valid() {
        let grammar = r#"
            start: WORD ("," WORD)*
            WORD: /\w+/
        "#;
        assert!(validate_lark_grammar(grammar).is_ok());
    }

    #[test]
    fn test_validate_lark_grammar_empty() {
        assert!(validate_lark_grammar("").is_err());
        assert!(validate_lark_grammar("   ").is_err());
    }

    #[test]
    fn test_validate_lark_grammar_unbalanced_braces() {
        assert!(validate_lark_grammar("start: {unclosed").is_err());
        assert!(validate_lark_grammar("start: }extra{").is_err());
    }

    #[test]
    fn test_validate_lark_grammar_no_rules() {
        assert!(validate_lark_grammar("no colons here").is_err());
    }

    #[test]
    fn test_validate_json_schema_valid() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        assert!(validate_json_schema(&schema).is_ok());
    }

    #[test]
    fn test_validate_json_schema_not_object() {
        let schema = serde_json::json!("string");
        assert!(validate_json_schema(&schema).is_err());
    }

    #[test]
    fn test_validate_json_schema_missing_type() {
        let schema = serde_json::json!({
            "properties": {
                "name": {"type": "string"}
            }
        });
        assert!(validate_json_schema(&schema).is_err());
    }

    #[test]
    fn test_grammar_constraint_regex() {
        let constraint = GrammarConstraint::regex(r"^\d{3}$");
        assert_eq!(constraint.format, GrammarFormat::Regex);
        assert_eq!(constraint.definition, r"^\d{3}$");
        assert!(constraint.input_property.is_none());
    }

    #[test]
    fn test_grammar_constraint_with_property() {
        let constraint = GrammarConstraint::regex(r"\d+").with_input_property("value");
        assert_eq!(constraint.input_property, Some("value".to_string()));
    }
}
