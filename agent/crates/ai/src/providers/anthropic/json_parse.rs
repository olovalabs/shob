//! Re-export of the JSON repair helpers.
//!
//! The implementation moved to [`crate::json_parse`] because it is a general
//! utility: assistant-message frames (`crate::frames`) need
//! `parse_streaming_json` and are compiled **without** the `providers` feature.
//! This module keeps the historical `providers::anthropic::json_parse` path
//! working for the SSE mapper and the other providers.

pub use crate::json_parse::{parse_json_with_repair, parse_streaming_json, repair_json};
