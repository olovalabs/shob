//! Optional binary payload support for high-frequency extension data.
//!
//! The binary codec is intentionally additive. Existing JSON `StbString` payloads
//! remain the compatibility format; callers opt into this module only after the
//! host and extension have negotiated the binary capability. Payloads are
//! length-bounded before decoding so this module is suitable for untrusted
//! extension input when the caller also enforces the surrounding frame limit.

use std::fmt;
use std::io::Cursor;

use serde::{de::DeserializeOwned, Serialize};

/// Default maximum encoded payload size (16 MiB).
pub const DEFAULT_MAX_PAYLOAD_LENGTH: usize = 16 * 1024 * 1024;

/// Bounds applied to a binary payload before encoding or decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinaryOptions {
    pub max_payload_length: usize,
}

impl Default for BinaryOptions {
    fn default() -> Self {
        Self {
            max_payload_length: DEFAULT_MAX_PAYLOAD_LENGTH,
        }
    }
}

/// Error returned by the binary codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryError {
    Encode(String),
    Decode(String),
    PayloadTooLarge { actual: usize, maximum: usize },
    TrailingBytes,
}

impl fmt::Display for BinaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(message) => write!(f, "binary encode failed: {message}"),
            Self::Decode(message) => write!(f, "binary decode failed: {message}"),
            Self::PayloadTooLarge { actual, maximum } => write!(
                f,
                "binary payload is {actual} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::TrailingBytes => f.write_str("binary payload contains trailing bytes"),
        }
    }
}

impl std::error::Error for BinaryError {}

/// Encode one typed value as a compact CBOR payload.
pub fn encode<T: Serialize>(value: &T, options: &BinaryOptions) -> Result<Vec<u8>, BinaryError> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(value, &mut bytes)
        .map_err(|error| BinaryError::Encode(error.to_string()))?;
    if bytes.len() > options.max_payload_length {
        return Err(BinaryError::PayloadTooLarge {
            actual: bytes.len(),
            maximum: options.max_payload_length,
        });
    }
    Ok(bytes)
}

/// Decode one typed value from a complete CBOR payload.
pub fn decode<T: DeserializeOwned>(
    bytes: &[u8],
    options: &BinaryOptions,
) -> Result<T, BinaryError> {
    if bytes.len() > options.max_payload_length {
        return Err(BinaryError::PayloadTooLarge {
            actual: bytes.len(),
            maximum: options.max_payload_length,
        });
    }

    let mut reader = Cursor::new(bytes);
    let value = ciborium::de::from_reader(&mut reader)
        .map_err(|error| BinaryError::Decode(error.to_string()))?;
    if reader.position() != bytes.len() as u64 {
        return Err(BinaryError::TrailingBytes);
    }
    Ok(value)
}

/// JSON-shaped convenience API for tool and event payloads that are still
/// represented by dynamic values at the product boundary.
#[cfg(feature = "json")]
pub fn encode_json(
    value: &serde_json::Value,
    options: &BinaryOptions,
) -> Result<Vec<u8>, BinaryError> {
    encode(value, options)
}

/// Decode a binary payload into a JSON value without going through a UTF-8
/// string or JSON parser.
#[cfg(feature = "json")]
pub fn decode_json(
    bytes: &[u8],
    options: &BinaryOptions,
) -> Result<serde_json::Value, BinaryError> {
    decode(bytes, options)
}

/// An owned byte buffer that can cross an ABI boundary as pointer plus length.
///
/// The allocation is always released by the producer's `FreeBytesFn`, just as
/// `StbString` is released by `FreeStringFn`. This type does not carry an
/// allocator or lifetime; those are part of the call contract.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StbBytes {
    pub ptr: *mut u8,
    pub len: usize,
}

// SAFETY: this is a raw pointer/length pair. It does not itself grant access
// to the allocation; the caller is responsible for the documented lifetime and
// ownership rules, exactly as for `StbString`.
unsafe impl Send for StbBytes {}
unsafe impl Sync for StbBytes {}

/// Producer-side deallocator for [`StbBytes`].
pub type FreeBytesFn = unsafe extern "C" fn(bytes: StbBytes);

impl StbBytes {
    /// The empty, non-owning value.
    pub const fn empty() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Transfer a boxed byte allocation into an ABI buffer.
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        if bytes.is_empty() {
            return Self::empty();
        }
        let boxed = bytes.into_boxed_slice();
        let len = boxed.len();
        let ptr = Box::into_raw(boxed) as *mut u8;
        Self { ptr, len }
    }

    /// Copy the borrowed ABI buffer into a Rust-owned vector.
    ///
    /// # Safety
    /// `ptr` must be valid for `len` initialized bytes in one allocation and
    /// remain readable for the duration of this call.
    pub unsafe fn to_vec(&self) -> Vec<u8> {
        if self.len == 0 || self.ptr.is_null() {
            return Vec::new();
        }
        // SAFETY: upheld by the caller's ABI buffer contract.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len).to_vec() }
    }

    /// Release the buffer through its producer-side deallocator.
    ///
    /// # Safety
    /// The buffer must be a live allocation produced by `free_fn`'s allocator,
    /// and ownership must be transferred exactly once.
    pub unsafe fn free_with(self, free_fn: Option<FreeBytesFn>) {
        if self.is_empty() {
            return;
        }
        if let Some(free_fn) = free_fn {
            // SAFETY: the caller supplies the matching producer and unique
            // ownership of this allocation.
            unsafe { free_fn(self) };
        }
    }
}

/// Default deallocator for buffers created by [`StbBytes::from_vec`].
///
/// # Safety
/// `bytes` must be the unique live allocation returned by `StbBytes::from_vec`.
pub unsafe extern "C" fn free_bytes(bytes: StbBytes) {
    if bytes.is_empty() || bytes.ptr.is_null() {
        return;
    }
    // SAFETY: the caller transfers the original boxed-slice allocation once.
    unsafe {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            bytes.ptr, bytes.len,
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_round_trip_uses_binary_payload() {
        let value = json!({
            "type": "tool_result",
            "tool": "read",
            "content": ["alpha", "beta"],
            "ok": true,
        });
        let bytes = encode_json(&value, &BinaryOptions::default()).unwrap();
        assert!(!bytes.is_empty());
        assert_ne!(bytes, serde_json::to_vec(&value).unwrap());
        assert_eq!(
            decode_json(&bytes, &BinaryOptions::default()).unwrap(),
            value
        );
    }

    #[test]
    fn typed_values_round_trip() {
        let value = (String::from("tool_result"), 42_u64, true);
        let bytes = encode(&value, &BinaryOptions::default()).unwrap();
        assert_eq!(
            decode::<(String, u64, bool)>(&bytes, &BinaryOptions::default()).unwrap(),
            value
        );
    }

    #[test]
    fn rejects_oversized_payloads() {
        let value = json!({"data": "x".repeat(64)});
        let options = BinaryOptions {
            max_payload_length: 8,
        };
        assert!(matches!(
            encode_json(&value, &options),
            Err(BinaryError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = encode_json(&json!({"ok": true}), &BinaryOptions::default()).unwrap();
        bytes.push(0);
        assert_eq!(
            decode_json(&bytes, &BinaryOptions::default()),
            Err(BinaryError::TrailingBytes)
        );
    }

    #[test]
    fn abi_buffer_owns_and_releases_bytes() {
        let buffer = StbBytes::from_vec(vec![1, 2, 3]);
        // SAFETY: `buffer` owns the live allocation returned above.
        assert_eq!(unsafe { buffer.to_vec() }, vec![1, 2, 3]);
        // SAFETY: the default deallocator matches `from_vec`.
        unsafe { buffer.free_with(Some(free_bytes)) };
    }
}
