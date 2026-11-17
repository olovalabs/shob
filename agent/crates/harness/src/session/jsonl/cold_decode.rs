//! Allocation-light recognition of assistant-frame rows during cold recovery.
//!
//! This is deliberately not a second codec: `None` means the caller must use
//! `codec::parse_mutation`, which owns both semantics and diagnostics. Successful
//! recognition validates the entire JSON document, including discarded payloads.

use std::{borrow::Cow, fmt};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::session::types::AssistantFrameOp;

#[derive(Debug)]
pub(super) struct ColdFrame<'a> {
    pub id: Cow<'a, str>,
    pub lane: Cow<'a, str>,
    pub run_id: Cow<'a, str>,
    pub seq: u64,
    pub timestamp: i64,
    pub stream_index: usize,
    pub op: AssistantFrameOp,
}

/// Recognize only frames whose bodies are unnecessary for indexing. In
/// particular, commit_stream needs its entryId and always uses the old codec.
pub(super) fn parse_cold_frame(line: &str) -> Option<ColdFrame<'_>> {
    // The canonical writer places kind first. Do not run a speculative serde
    // pass (and allocate its error) for every ordinary message. This is only a
    // rejection hint: unusual whitespace/order/duplicate discriminators still
    // go through the recognizer or the authoritative complete codec.
    let prefix = line.trim_start();
    if prefix.starts_with("{\"kind\":\"entry\"")
        || prefix.starts_with("{\"kind\":\"lane\"")
        || prefix.starts_with("{\"kind\":\"fact\"")
    {
        return None;
    }
    let mut deserializer = serde_json::Deserializer::from_str(line);
    let frame = ColdFrame::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    Some(frame)
}

/// Unlike Deserialize for Cow itself, preserve strings borrowed from the input
/// whenever serde_json can expose them without unescaping.
struct WireString<'a>(Cow<'a, str>);

impl<'de> Deserialize<'de> for WireString<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StringVisitor;
        impl<'de> Visitor<'de> for StringVisitor {
            type Value = WireString<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
                Ok(WireString(Cow::Borrowed(value)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(WireString(Cow::Owned(value.to_owned())))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(WireString(Cow::Owned(value)))
            }
        }
        deserializer.deserialize_str(StringVisitor)
    }
}

const RAW_VALUE_KEY: &str = "$serde_json::private::RawValue";

#[derive(Clone, Copy)]
#[repr(u16)]
enum Field {
    Unknown = 0,
    RawValue = 1 << 10,
    Kind = 1 << 0,
    Type = 1 << 1,
    Id = 1 << 2,
    Lane = 1 << 3,
    RunId = 1 << 4,
    Seq = 1 << 5,
    Timestamp = 1 << 6,
    StreamIndex = 1 << 7,
    Op = 1 << 8,
    Frame = 1 << 9,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor;
        impl Visitor<'_> for FieldVisitor {
            type Value = Field;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object key")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Field, E> {
                Ok(match value {
                    "kind" => Field::Kind,
                    "type" => Field::Type,
                    "id" => Field::Id,
                    "lane" => Field::Lane,
                    "runId" => Field::RunId,
                    "seq" => Field::Seq,
                    "timestamp" => Field::Timestamp,
                    "streamIndex" => Field::StreamIndex,
                    "op" => Field::Op,
                    "frame" => Field::Frame,
                    RAW_VALUE_KEY => Field::RawValue,
                    _ => Field::Unknown,
                })
            }
        }
        deserializer.deserialize_identifier(FieldVisitor)
    }
}

/// Go through deserialize_any rather than deserialize_u64/i64: the old codec
/// first builds Value, so exponent/decimal forms (and -0) are floats even when
/// numerically integral. Typed integer deserialization need not behave the same.
enum WireInteger {
    Unsigned(u64),
    Signed(i64),
}

impl WireInteger {
    fn unsigned(self) -> Option<u64> {
        match self {
            Self::Unsigned(value) => Some(value),
            Self::Signed(value) => u64::try_from(value).ok(),
        }
    }

    fn signed(self) -> Option<i64> {
        match self {
            Self::Unsigned(value) => i64::try_from(value).ok(),
            Self::Signed(value) => Some(value),
        }
    }
}

impl<'de> Deserialize<'de> for WireInteger {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IntegerVisitor;
        impl Visitor<'_> for IntegerVisitor {
            type Value = WireInteger;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an integer representable by a JSON integer")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(WireInteger::Unsigned(value))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(WireInteger::Signed(value))
            }
        }
        deserializer.deserialize_any(IntegerVisitor)
    }
}

/// Validate, but never retain, an arbitrary JSON value. Do not replace this
/// with IgnoredAny/RawValue: those can skip number-range validation that Value
/// performs in the reference codec. The normal serde_json recursion limit and
/// string/Unicode/number parsing all remain active here.
struct Discard;

impl<'de> Deserialize<'de> for Discard {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DiscardVisitor;
        impl<'de> Visitor<'de> for DiscardVisitor {
            type Value = Discard;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Discard, E> {
                Ok(Discard)
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Discard, E> {
                Ok(Discard)
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Discard, E> {
                Ok(Discard)
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Discard, E> {
                Ok(Discard)
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Discard, E> {
                if value.is_finite() {
                    Ok(Discard)
                } else {
                    Err(E::custom("non-finite JSON number"))
                }
            }

            fn visit_str<E: de::Error>(self, _: &str) -> Result<Discard, E> {
                Ok(Discard)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Discard, A::Error> {
                while seq.next_element::<Discard>()?.is_some() {}
                Ok(Discard)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Discard, A::Error> {
                while let Some(field) = map.next_key::<Field>()? {
                    if matches!(field, Field::RawValue) {
                        // serde_json::Value gives this key special meaning when
                        // it is the first decoded key of an object. The
                        // optimizer must never approximate that protocol; hand
                        // the complete line to the authoritative decoder.
                        return Err(de::Error::custom("serde_json raw value key"));
                    }
                    map.next_value::<Discard>()?;
                }
                Ok(Discard)
            }
        }
        deserializer.deserialize_any(DiscardVisitor)
    }
}

impl<'de> Deserialize<'de> for ColdFrame<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FrameVisitor;
        impl<'de> Visitor<'de> for FrameVisitor {
            type Value = ColdFrame<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an assistant-frame record")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = 0u16;
                let (mut id, mut lane, mut run_id) = (None, None, None);
                let (mut seq, mut timestamp, mut stream_index, mut op) = (None, None, None, None);
                while let Some(field) = map.next_key::<Field>()? {
                    let bit = field as u16;
                    // Value has last-wins semantics; serde's flattened struct
                    // processing does not provide the same duplicate handling.
                    // Never try to resolve that ambiguity in this fast path.
                    if seen & bit != 0 {
                        return Err(de::Error::custom("duplicate known frame field"));
                    }
                    seen |= bit;
                    match field {
                        Field::Kind => {
                            if map.next_value::<WireString<'de>>()?.0 != "record" {
                                return Err(de::Error::custom("not a record"));
                            }
                        }
                        Field::Type => {
                            if map.next_value::<WireString<'de>>()?.0 != "assistant_frame" {
                                return Err(de::Error::custom("not an assistant frame"));
                            }
                        }
                        Field::Id => id = Some(map.next_value::<WireString<'de>>()?.0),
                        Field::Lane => lane = Some(map.next_value::<WireString<'de>>()?.0),
                        Field::RunId => run_id = Some(map.next_value::<WireString<'de>>()?.0),
                        Field::Seq => {
                            seq = Some(
                                map.next_value::<WireInteger>()?
                                    .unsigned()
                                    .filter(|value| *value > 0)
                                    .ok_or_else(|| de::Error::custom("invalid seq"))?,
                            );
                        }
                        Field::Timestamp => {
                            // RecordBase, unlike entry timestamps, allows negatives.
                            timestamp = Some(
                                map.next_value::<WireInteger>()?
                                    .signed()
                                    .ok_or_else(|| de::Error::custom("invalid timestamp"))?,
                            );
                        }
                        Field::StreamIndex => {
                            stream_index = Some(
                                map.next_value::<WireInteger>()?
                                    .unsigned()
                                    .and_then(|value| usize::try_from(value).ok())
                                    .ok_or_else(|| de::Error::custom("invalid streamIndex"))?,
                            );
                        }
                        Field::Op => {
                            op = Some(match map.next_value::<WireString<'de>>()?.0.as_ref() {
                                "append" => AssistantFrameOp::Append,
                                "clear_stream" => AssistantFrameOp::ClearStream,
                                "clear_run" => AssistantFrameOp::ClearRun,
                                // The reference enum also accepts some map forms;
                                // leave all such uncommon representations to it.
                                _ => return Err(de::Error::custom("not a cold frame operation")),
                            });
                        }
                        Field::Frame | Field::Unknown => {
                            map.next_value::<Discard>()?;
                        }
                        Field::RawValue => {
                            // Field keys are decoded, rather than searched in
                            // the source text, so escaped spellings also
                            // follow the canonical raw_value behavior.
                            return Err(de::Error::custom("serde_json raw value key"));
                        }
                    }
                }
                if seen & ((1 << 9) - 1) != (1 << 9) - 1 {
                    return Err(de::Error::custom("missing required frame field"));
                }
                Ok(ColdFrame {
                    id: id.ok_or_else(|| de::Error::missing_field("id"))?,
                    lane: lane.ok_or_else(|| de::Error::missing_field("lane"))?,
                    run_id: run_id.ok_or_else(|| de::Error::missing_field("runId"))?,
                    seq: seq.ok_or_else(|| de::Error::missing_field("seq"))?,
                    timestamp: timestamp.ok_or_else(|| de::Error::missing_field("timestamp"))?,
                    stream_index: stream_index
                        .ok_or_else(|| de::Error::missing_field("streamIndex"))?,
                    op: op.ok_or_else(|| de::Error::missing_field("op"))?,
                })
            }
        }
        // deserialize_any keeps the root object on the exact Value recursion
        // accounting path too; non-object roots fail via the visitor defaults.
        deserializer.deserialize_any(FrameVisitor)
    }
}

#[cfg(test)]
#[path = "cold_decode_tests.rs"]
mod tests;
