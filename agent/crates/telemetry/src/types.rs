//! Span attribute primitives.

use std::collections::BTreeMap;

/// A single attribute value recorded on a span. Mirrors the TS `AttributeValue`
/// union (string | number | boolean | string[]).
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    String(String),
    Number(f64),
    Boolean(bool),
    StringSlice(Vec<String>),
}

/// An ordered map of attribute name → value. `BTreeMap` for deterministic
/// serialization (matches the JSON object ordering the TS layer emits).
pub type SpanAttributes = BTreeMap<String, AttributeValue>;

/// Options for starting a span. Mirrors `SpanOptions { name, attributes }`.
#[derive(Debug, Clone, Default)]
pub struct SpanOptions {
    pub name: String,
    pub attributes: SpanAttributes,
}

impl SpanOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            attributes: SpanAttributes::new(),
        }
    }

    pub fn with_attribute(
        mut self,
        key: impl Into<String>,
        value: impl Into<AttributeValue>,
    ) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }
}

impl From<&str> for AttributeValue {
    fn from(s: &str) -> Self {
        AttributeValue::String(s.to_string())
    }
}

impl From<String> for AttributeValue {
    fn from(s: String) -> Self {
        AttributeValue::String(s)
    }
}

impl From<bool> for AttributeValue {
    fn from(b: bool) -> Self {
        AttributeValue::Boolean(b)
    }
}

impl From<i64> for AttributeValue {
    fn from(n: i64) -> Self {
        AttributeValue::Number(n as f64)
    }
}

impl From<u64> for AttributeValue {
    fn from(n: u64) -> Self {
        AttributeValue::Number(n as f64)
    }
}

impl From<f64> for AttributeValue {
    fn from(n: f64) -> Self {
        AttributeValue::Number(n)
    }
}

/// Terminal status of a span.
#[derive(Debug, Clone, PartialEq)]
pub enum SpanStatus {
    /// Span completed successfully.
    Ok,
    /// Span ended in an error; carries an error `name` + `message`.
    Error { name: String, message: String },
}

impl SpanStatus {
    pub fn ok() -> Self {
        SpanStatus::Ok
    }

    pub fn error(name: impl Into<String>, message: impl Into<String>) -> Self {
        SpanStatus::Error {
            name: name.into(),
            message: message.into(),
        }
    }
}
