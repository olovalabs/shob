//! Binary protocol codec: frame + CBOR encode/decode of protocol messages.
//!
//! Port of the upstream `packages/protocol/` module.
//! CBOR layers so callers send/receive `serde_json::Value` protocol messages
//! over a length-prefixed CBOR stream.

use serde_json::Value;

use super::cbor::{self, CborError, CborOptions};
use super::framing::{self, FrameDecoder, FrameError, DEFAULT_MAX_FRAME_LENGTH};

/// Protocol framing/validation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    Frame(String),
    Cbor(String),
    Encode(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(m) => write!(f, "frame: {m}"),
            Self::Cbor(m) => write!(f, "cbor: {m}"),
            Self::Encode(m) => write!(f, "encode: {m}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<FrameError> for ProtocolError {
    fn from(e: FrameError) -> Self {
        Self::Frame(e.0)
    }
}

impl From<CborError> for ProtocolError {
    fn from(e: CborError) -> Self {
        Self::Cbor(e.0)
    }
}

/// Encode a protocol message into a single framed CBOR payload.
pub fn encode_message(value: &Value) -> Result<Vec<u8>, ProtocolError> {
    encode_message_with(value, &CborOptions::default(), DEFAULT_MAX_FRAME_LENGTH)
}

/// Encode with explicit limits (native `encodeProtocolMessage`).
pub fn encode_message_with(
    value: &Value,
    cbor_options: &CborOptions,
    max_frame_length: usize,
) -> Result<Vec<u8>, ProtocolError> {
    let payload = cbor::encode(value, cbor_options)?;
    if payload.len() > max_frame_length {
        return Err(ProtocolError::Encode(format!(
            "encoded frame {} exceeds max frame length {max_frame_length}",
            payload.len()
        )));
    }
    Ok(framing::encode_frame(&payload)?)
}

/// Incremental framed-CBOR decoder: feed bytes, get protocol messages.
pub struct MessageDecoder {
    frames: FrameDecoder,
    cbor_options: CborOptions,
}

impl MessageDecoder {
    pub fn new(max_frame_length: usize, cbor_options: CborOptions) -> Self {
        Self {
            frames: FrameDecoder::new(max_frame_length),
            cbor_options,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MAX_FRAME_LENGTH, CborOptions::default())
    }

    /// Feed a chunk; returns any complete protocol messages it produced.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, ProtocolError> {
        let payloads = self.frames.push(chunk)?;
        let mut messages = Vec::with_capacity(payloads.len());
        for payload in payloads {
            messages.push(cbor::decode(&payload, &self.cbor_options)?);
        }
        Ok(messages)
    }

    /// Signal end of stream (errors if a partial frame is buffered).
    pub fn end(&mut self) -> Result<(), ProtocolError> {
        self.frames.end()?;
        Ok(())
    }
}

/// Decode a single complete framed payload (native `decodeProtocolMessage`).
pub fn decode_message(
    frame: &[u8],
    cbor_options: &CborOptions,
    max_frame_length: usize,
) -> Result<Value, ProtocolError> {
    framing::assert_complete_frame(frame, max_frame_length)?;
    let payload = &frame[framing::FRAME_HEADER_LENGTH..];
    Ok(cbor::decode(payload, cbor_options)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_message() {
        let message = json!({ "type": "hello", "version": 1, "tools": ["read", "write"] });
        let frame = encode_message(&message).unwrap();
        let decoded =
            decode_message(&frame, &CborOptions::default(), DEFAULT_MAX_FRAME_LENGTH).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn decoder_stream() {
        let a = encode_message(&json!({ "type": "a" })).unwrap();
        let b = encode_message(&json!({ "type": "b" })).unwrap();
        let mut decoder = MessageDecoder::with_defaults();
        assert!(decoder.push(&a[..3]).unwrap().is_empty());
        let mut all = Vec::new();
        all.extend(decoder.push(&a[3..]).unwrap());
        all.extend(decoder.push(&b).unwrap());
        assert_eq!(all, vec![json!({ "type": "a" }), json!({ "type": "b" })]);
        decoder.end().unwrap();
    }

    #[test]
    fn rejects_oversized_encoding() {
        let big = json!({ "type": "x", "data": "a".repeat(64) });
        assert!(encode_message_with(&big, &CborOptions::default(), 8).is_err());
    }
}
