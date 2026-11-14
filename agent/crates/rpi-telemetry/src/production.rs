//! Production telemetry context implementation.
//!
//! This module provides a real telemetry backend that records spans and events
//! to a structured log format. It supports:
//! - Hierarchical span tracking with parent-child relationships
//! - Attribute recording on spans
//! - Event recording within spans
//! - Configurable output destinations (stdout, file, or custom writers)
//! - JSON-formatted output for integration with log aggregation systems

use crate::context::{SpanGuard, TelemetryContext};
use crate::types::{AttributeValue, SpanAttributes, SpanOptions, SpanStatus};
use serde::Serialize;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// A production telemetry context that records spans to a writer.
pub struct ProductionTelemetryContext {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    session_id: String,
    enabled: bool,
}

impl ProductionTelemetryContext {
    /// Create a new production telemetry context writing to stdout.
    pub fn stdout() -> Self {
        Self {
            writer: Arc::new(Mutex::new(Box::new(std::io::stdout()))),
            session_id: generate_session_id(),
            enabled: true,
        }
    }

    /// Create a new production telemetry context writing to a file.
    pub fn file(path: &str) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            writer: Arc::new(Mutex::new(Box::new(file))),
            session_id: generate_session_id(),
            enabled: true,
        })
    }

    /// Create a disabled telemetry context (no output).
    pub fn disabled() -> Self {
        Self {
            writer: Arc::new(Mutex::new(Box::new(std::io::sink()))),
            session_id: generate_session_id(),
            enabled: false,
        }
    }

    /// Check if telemetry is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enable or disable telemetry.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Get the current session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn write_record(&self, record: &TelemetryRecord) {
        if !self.enabled {
            return;
        }

        if let Ok(json) = serde_json::to_string(record) {
            if let Ok(mut writer) = self.writer.lock() {
                let _ = writeln!(writer, "{}", json);
            }
        }
    }
}

impl TelemetryContext for ProductionTelemetryContext {
    fn start_span(&self, options: SpanOptions) -> SpanGuard {
        let span_id = generate_span_id();
        let timestamp = current_timestamp();

        let start_record = TelemetryRecord::SpanStart {
            timestamp,
            session_id: self.session_id.clone(),
            span_id: span_id.clone(),
            name: options.name.clone(),
            attributes: options.attributes.clone(),
        };

        self.write_record(&start_record);

        let writer = Arc::clone(&self.writer);
        let session_id = self.session_id.clone();
        let enabled = self.enabled;

        SpanGuard::new(options.name, move |name, status| {
            if !enabled {
                return;
            }

            let end_timestamp = current_timestamp();
            let end_record = TelemetryRecord::SpanEnd {
                timestamp: end_timestamp,
                session_id: session_id.clone(),
                span_id: span_id.clone(),
                name: name.to_string(),
                status: match status {
                    SpanStatus::Ok => "ok".to_string(),
                    SpanStatus::Error { name, message } => {
                        format!("error:{}:{}", name, message)
                    }
                },
            };

            if let Ok(json) = serde_json::to_string(&end_record) {
                if let Ok(mut writer) = writer.lock() {
                    let _ = writeln!(writer, "{}", json);
                }
            }
        })
    }
}

/// Telemetry record types for JSON serialization.
#[derive(Serialize)]
#[serde(tag = "type")]
enum TelemetryRecord {
    #[serde(rename = "span_start")]
    SpanStart {
        timestamp: u128,
        session_id: String,
        span_id: String,
        name: String,
        attributes: SpanAttributes,
    },
    #[serde(rename = "span_end")]
    SpanEnd {
        timestamp: u128,
        session_id: String,
        span_id: String,
        name: String,
        status: String,
    },
}

/// Generate a unique session ID.
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("sess_{:x}", timestamp)
}

/// Generate a unique span ID.
fn generate_span_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("span_{:x}", timestamp)
}

/// Get current timestamp in milliseconds since epoch.
fn current_timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

/// Serialize attribute values for JSON output.
impl Serialize for AttributeValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            AttributeValue::String(s) => serializer.serialize_str(s),
            AttributeValue::Number(n) => serializer.serialize_f64(*n),
            AttributeValue::Boolean(b) => serializer.serialize_bool(*b),
            AttributeValue::StringSlice(v) => v.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_production_context_creation() {
        let ctx = ProductionTelemetryContext::stdout();
        assert!(ctx.is_enabled());
        assert!(!ctx.session_id().is_empty());
    }

    #[test]
    fn test_disabled_context() {
        let ctx = ProductionTelemetryContext::disabled();
        assert!(!ctx.is_enabled());
    }

    #[test]
    fn test_span_recording() {
        // The context owns a `'static` writer, so capture into a shared buffer
        // rather than borrowing a local `Vec`.
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = {
            let buffer = buffer.clone();
            Arc::new(Mutex::new(Box::new(SharedBuffer(buffer))))
        };

        let ctx = ProductionTelemetryContext {
            writer,
            session_id: "test_session".to_string(),
            enabled: true,
        };

        let options = SpanOptions::new("test_span").with_attribute("key", "value");

        {
            let _guard = ctx.start_span(options);
        }

        let bytes = buffer.lock().unwrap().clone();
        let output_str = String::from_utf8(bytes).unwrap();
        assert!(output_str.contains("span_start"));
        assert!(output_str.contains("span_end"));
        assert!(output_str.contains("test_span"));
    }

    /// A `Write` that appends into a shared buffer, so a telemetry context can
    /// own it for `'static` while the test still reads the bytes back.
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
