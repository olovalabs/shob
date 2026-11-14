//! Strict CBOR codec for protocol values.
//!
//! Port of the upstream `packages/protocol/` module.
//! carries JSON-shaped values (null, bool, number, string, array, object), so
//! this codec implements exactly that subset of CBOR (RFC 8949) with the
//! defensive limits native uses for untrusted payloads:
//!
//! * a maximum encoded byte length / string length ([`DEFAULT_MAX_CBOR_BYTE_LENGTH`]),
//! * a maximum container length ([`DEFAULT_MAX_CBOR_CONTAINER_LENGTH`]), and
//! * a maximum nesting depth ([`DEFAULT_MAX_CBOR_DEPTH`]).

use std::fmt;

use serde_json::{Map, Number, Value};

/// Native `DEFAULT_MAX_CBOR_BYTE_LENGTH` (16 MiB).
pub const DEFAULT_MAX_CBOR_BYTE_LENGTH: usize = 16 * 1024 * 1024;
/// Native `DEFAULT_MAX_CBOR_CONTAINER_LENGTH`.
pub const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: usize = 1_000_000;
/// Native `DEFAULT_MAX_CBOR_DEPTH`.
pub const DEFAULT_MAX_CBOR_DEPTH: usize = 64;

/// CBOR error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CborError(pub String);

impl fmt::Display for CborError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CborError {}

/// Codec limits (native `ResolvedCborOptions`).
#[derive(Debug, Clone, Copy)]
pub struct CborOptions {
    pub max_byte_length: usize,
    pub max_container_length: usize,
    pub max_depth: usize,
}

impl Default for CborOptions {
    fn default() -> Self {
        Self {
            max_byte_length: DEFAULT_MAX_CBOR_BYTE_LENGTH,
            max_container_length: DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
            max_depth: DEFAULT_MAX_CBOR_DEPTH,
        }
    }
}

fn write_head(out: &mut Vec<u8>, major: u8, value: u64) {
    let m = major << 5;
    if value < 24 {
        out.push(m | value as u8);
    } else if value <= u8::MAX as u64 {
        out.push(m | 24);
        out.push(value as u8);
    } else if value <= u16::MAX as u64 {
        out.push(m | 25);
        out.extend_from_slice(&(value as u16).to_be_bytes());
    } else if value <= u32::MAX as u64 {
        out.push(m | 26);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn encode_value(
    value: &Value,
    out: &mut Vec<u8>,
    options: &CborOptions,
    depth: usize,
) -> Result<(), CborError> {
    if depth > options.max_depth {
        return Err(CborError("CBOR nesting depth exceeded".into()));
    }
    match value {
        Value::Null => out.push(0xf6),
        Value::Bool(false) => out.push(0xf4),
        Value::Bool(true) => out.push(0xf5),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                write_head(out, 0, u);
            } else if let Some(i) = n.as_i64() {
                // Negative: major 1 encodes -1 - value.
                write_head(out, 1, (-(i as i128) - 1) as u64);
            } else if let Some(f) = n.as_f64() {
                out.push(0xfb);
                out.extend_from_slice(&f.to_be_bytes());
            } else {
                return Err(CborError("unsupported number".into()));
            }
        }
        Value::String(s) => {
            let bytes = s.as_bytes();
            if bytes.len() > options.max_byte_length {
                return Err(CborError("CBOR string exceeds max byte length".into()));
            }
            write_head(out, 3, bytes.len() as u64);
            out.extend_from_slice(bytes);
        }
        Value::Array(items) => {
            if items.len() > options.max_container_length {
                return Err(CborError("CBOR array exceeds max container length".into()));
            }
            write_head(out, 4, items.len() as u64);
            for item in items {
                encode_value(item, out, options, depth + 1)?;
            }
        }
        Value::Object(map) => {
            if map.len() > options.max_container_length {
                return Err(CborError("CBOR map exceeds max container length".into()));
            }
            write_head(out, 5, map.len() as u64);
            for (key, item) in map {
                if key.len() > options.max_byte_length {
                    return Err(CborError("CBOR string exceeds max byte length".into()));
                }
                write_head(out, 3, key.len() as u64);
                out.extend_from_slice(key.as_bytes());
                encode_value(item, out, options, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Encode a JSON value to CBOR bytes.
pub fn encode(value: &Value, options: &CborOptions) -> Result<Vec<u8>, CborError> {
    let mut out = Vec::new();
    encode_value(value, &mut out, options, 0)?;
    if out.len() > options.max_byte_length {
        return Err(CborError("CBOR payload exceeds max byte length".into()));
    }
    Ok(out)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CborError> {
        if self.pos + n > self.bytes.len() {
            return Err(CborError("unexpected end of CBOR input".into()));
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, CborError> {
        Ok(self.take(1)?[0])
    }

    fn read_len(&mut self, additional: u8) -> Result<Option<u64>, CborError> {
        match additional {
            0..=23 => Ok(Some(additional as u64)),
            24 => Ok(Some(self.u8()? as u64)),
            25 => {
                let b = self.take(2)?;
                Ok(Some(u16::from_be_bytes([b[0], b[1]]) as u64))
            }
            26 => {
                let b = self.take(4)?;
                Ok(Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64))
            }
            27 => {
                let b = self.take(8)?;
                Ok(Some(u64::from_be_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ])))
            }
            _ => Ok(None),
        }
    }
}

fn decode_value(
    reader: &mut Reader<'_>,
    options: &CborOptions,
    depth: usize,
) -> Result<Value, CborError> {
    if depth > options.max_depth {
        return Err(CborError("CBOR nesting depth exceeded".into()));
    }
    let initial = reader.u8()?;
    let major = initial >> 5;
    let additional = initial & 0x1f;
    match major {
        0 => {
            let n = reader
                .read_len(additional)?
                .ok_or_else(|| CborError("invalid CBOR integer".into()))?;
            Ok(Value::Number(Number::from(n)))
        }
        1 => {
            let n = reader
                .read_len(additional)?
                .ok_or_else(|| CborError("invalid CBOR integer".into()))?;
            if n > i64::MAX as u64 {
                return Err(CborError("CBOR negative integer out of range".into()));
            }
            Ok(Value::Number(Number::from(-(n as i64) - 1)))
        }
        2 => {
            // Byte string: represent as an array of numbers (protocol values
            // never carry raw bytes; this keeps the codec total).
            let len = reader
                .read_len(additional)?
                .ok_or_else(|| CborError("invalid CBOR byte string".into()))?
                as usize;
            if len > options.max_byte_length {
                return Err(CborError("CBOR string exceeds max byte length".into()));
            }
            let bytes = reader.take(len)?;
            Ok(Value::Array(
                bytes
                    .iter()
                    .map(|b| Value::Number(Number::from(*b)))
                    .collect(),
            ))
        }
        3 => {
            let len = reader
                .read_len(additional)?
                .ok_or_else(|| CborError("invalid CBOR text string".into()))?
                as usize;
            if len > options.max_byte_length {
                return Err(CborError("CBOR string exceeds max byte length".into()));
            }
            let bytes = reader.take(len)?;
            let text = std::str::from_utf8(bytes)
                .map_err(|_| CborError("invalid UTF-8 in CBOR text string".into()))?;
            Ok(Value::String(text.to_string()))
        }
        4 => {
            let len = reader
                .read_len(additional)?
                .ok_or_else(|| CborError("invalid CBOR array".into()))?
                as usize;
            if len > options.max_container_length {
                return Err(CborError("CBOR array exceeds max container length".into()));
            }
            let mut items = Vec::with_capacity(len.min(4096));
            for _ in 0..len {
                items.push(decode_value(reader, options, depth + 1)?);
            }
            Ok(Value::Array(items))
        }
        5 => {
            let len = reader
                .read_len(additional)?
                .ok_or_else(|| CborError("invalid CBOR map".into()))?
                as usize;
            if len > options.max_container_length {
                return Err(CborError("CBOR map exceeds max container length".into()));
            }
            let mut map = Map::new();
            for _ in 0..len {
                let key = decode_value(reader, options, depth + 1)?;
                let Value::String(key) = key else {
                    return Err(CborError("CBOR map key must be a text string".into()));
                };
                let value = decode_value(reader, options, depth + 1)?;
                map.insert(key, value);
            }
            Ok(Value::Object(map))
        }
        6 => {
            // Tag: ignore, decode the tagged value.
            let _ = reader.read_len(additional)?;
            decode_value(reader, options, depth)
        }
        7 => match additional {
            20 => Ok(Value::Bool(false)),
            21 => Ok(Value::Bool(true)),
            22 => Ok(Value::Null),
            26 => {
                let b = reader.take(4)?;
                let f = f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64;
                Ok(Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null))
            }
            27 => {
                let b = reader.take(8)?;
                let f = f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                Ok(Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null))
            }
            _ => Err(CborError("unsupported CBOR simple value".into())),
        },
        _ => Err(CborError("unsupported CBOR major type".into())),
    }
}

/// Decode CBOR bytes into a JSON value.
pub fn decode(bytes: &[u8], options: &CborOptions) -> Result<Value, CborError> {
    if bytes.len() > options.max_byte_length {
        return Err(CborError("CBOR payload exceeds max byte length".into()));
    }
    let mut reader = Reader { bytes, pos: 0 };
    let value = decode_value(&mut reader, options, 0)?;
    if reader.pos != bytes.len() {
        return Err(CborError("trailing bytes after CBOR value".into()));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(value: Value) {
        let options = CborOptions::default();
        let bytes = encode(&value, &options).unwrap();
        let decoded = decode(&bytes, &options).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn round_trips_scalars() {
        round_trip(json!(null));
        round_trip(json!(true));
        round_trip(json!(false));
        round_trip(json!(0));
        round_trip(json!(23));
        round_trip(json!(24));
        round_trip(json!(300));
        round_trip(json!(70000));
        round_trip(json!(5_000_000_000i64));
        round_trip(json!(-1));
        round_trip(json!(-300));
        round_trip(json!("hello"));
        round_trip(json!(1.5));
    }

    #[test]
    fn round_trips_containers() {
        round_trip(json!([1, "two", [3, 4], { "a": true }]));
        round_trip(json!({ "type": "hello", "nested": { "x": [1, 2, 3] } }));
    }

    #[test]
    fn rejects_deep_nesting() {
        let options = CborOptions {
            max_depth: 3,
            ..Default::default()
        };
        let deep = json!([[[[1]]]]);
        assert!(encode(&deep, &options).is_err());
    }

    #[test]
    fn rejects_trailing_bytes() {
        let options = CborOptions::default();
        let mut bytes = encode(&json!(1), &options).unwrap();
        bytes.push(0);
        assert!(decode(&bytes, &options).is_err());
    }

    #[test]
    fn rejects_truncated() {
        let options = CborOptions::default();
        let bytes = encode(&json!("abcdef"), &options).unwrap();
        assert!(decode(&bytes[..3], &options).is_err());
    }
}
