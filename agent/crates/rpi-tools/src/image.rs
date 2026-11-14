//! Mirrors `packages/agent/src/harness/tools/image.ts` — image MIME detection
//! from byte signatures and base64 encoding.
//!
//! Detection is signature-based, NOT extension-based. The TS uses a manual
//! base64 encoder with no trailing newline; the Rust port uses `base64`'s
//! `STANDARD_NO_LINE_WRAP` engine (equivalent).

use base64::{engine::general_purpose::STANDARD, Engine};

/// Detect a supported image MIME type from a byte signature. Returns `None`
/// for unrecognized data, the JPEG-XT variant, animated PNG, or invalid BMP.
///
/// Supported: JPEG, PNG (non-animated), GIF, WEBP, BMP.
pub fn detect_supported_image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if is_jpeg(bytes) {
        return Some("image/jpeg");
    }
    if is_png(bytes) {
        return Some("image/png");
    }
    if starts_with_ascii_at(bytes, 0, "GIF") && bytes.len() >= 6 {
        // GIF87a / GIF89a — signature only.
        return Some("image/gif");
    }
    if starts_with_ascii_at(bytes, 0, "RIFF") && starts_with_ascii_at(bytes, 8, "WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii_at(bytes, 0, "BM") && is_bmp(bytes) {
        return Some("image/bmp");
    }
    None
}

/// Encode bytes to standard base64 with no line wrapping. Mirrors `encodeBase64`
/// (no trailing newline).
pub fn encode_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

// ----------------------------------------------------------------------------
// Signature helpers — all bounds-safe (OOB reads treat as 0 / false).
// ----------------------------------------------------------------------------

fn read_u16_le(bytes: &[u8], offset: usize) -> u16 {
    if offset + 1 >= bytes.len() {
        return 0;
    }
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32_be(bytes: &[u8], offset: usize) -> u32 {
    if offset + 3 >= bytes.len() {
        return 0;
    }
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    if offset + 3 >= bytes.len() {
        return 0;
    }
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn starts_with(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes.len() >= prefix.len() && &bytes[..prefix.len()] == prefix
}

fn starts_with_ascii_at(bytes: &[u8], offset: usize, ascii: &str) -> bool {
    let pat = ascii.as_bytes();
    if offset + pat.len() > bytes.len() {
        return false;
    }
    &bytes[offset..offset + pat.len()] == pat
}

fn is_jpeg(bytes: &[u8]) -> bool {
    // FF D8 FF, but NOT the JPEG-XT variant (FF D8 FF F7).
    starts_with(bytes, &[0xFF, 0xD8, 0xFF]) && bytes.get(3).copied() != Some(0xF7)
}

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

fn is_png(bytes: &[u8]) -> bool {
    if !starts_with(bytes, &PNG_SIGNATURE) {
        return false;
    }
    // First chunk must be IHDR of length 13.
    bytes.len() >= 16
        && read_u32_be(bytes, 8) == 13
        && starts_with_ascii_at(bytes, 12, "IHDR")
        && !is_animated_png(bytes)
}

/// Walk PNG chunks; return `true` if an `acTL` chunk appears before `IDAT`.
/// Stops on overflow / zero-length-next to prevent infinite loops.
fn is_animated_png(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let mut offset = 8usize;
    loop {
        // 4-byte length (BE) + 4-byte type + payload + 4-byte CRC.
        if offset + 8 > bytes.len() {
            return false;
        }
        let length = read_u32_be(bytes, offset) as usize;
        let type_offset = offset + 4;
        if type_offset + 4 > bytes.len() {
            return false;
        }
        let chunk_type = &bytes[type_offset..type_offset + 4];
        if chunk_type == b"acTL" {
            return true;
        }
        if chunk_type == b"IDAT" {
            return false;
        }
        // next chunk: length field (4) + type (4) + payload (length) + CRC (4)
        let next = offset + 4 + 4 + length + 4;
        if next <= offset {
            // overflow / zero progress guard
            return false;
        }
        offset = next;
    }
}

fn is_bmp(bytes: &[u8]) -> bool {
    if bytes.len() < 26 {
        return false;
    }
    let declared_file_size = read_u32_le(bytes, 2);
    let pixel_data_offset = read_u32_le(bytes, 10) as usize;
    let dib_header_size = read_u32_le(bytes, 14) as usize;

    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size as usize {
        return false;
    }

    let (planes, bpp) = if dib_header_size == 12 {
        // BITMAPCOREHEADER: planes @ 22, bpp @ 24 (both LE u16).
        (read_u16_le(bytes, 22), read_u16_le(bytes, 24))
    } else if (12..=124).contains(&dib_header_size) {
        if bytes.len() < 30 {
            return false;
        }
        (read_u16_le(bytes, 26), read_u16_le(bytes, 28))
    } else {
        return false;
    };

    planes == 1 && matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_jpeg() {
        let bytes = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x00];
        assert_eq!(detect_supported_image_mime_type(&bytes), Some("image/jpeg"));
    }

    #[test]
    fn rejects_jpeg_xt_variant() {
        // FF D8 FF F7 → JPEG-XT, NOT supported.
        let bytes = [0xFF, 0xD8, 0xFF, 0xF7, 0x00, 0x00];
        assert_eq!(detect_supported_image_mime_type(&bytes), None);
    }

    #[test]
    fn detects_png_static() {
        // PNG signature + IHDR chunk (length 13) + 4 bytes of IHDR data + fake
        // IDAT chunk to prove it's not animated.
        let mut bytes = PNG_SIGNATURE.to_vec();
        // length 13 (BE) + "IHDR" + 13 bytes payload + CRC(4)
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&[0u8; 13]);
        bytes.extend_from_slice(&[0u8; 4]); // CRC placeholder
                                            // IDAT chunk (length 0) to terminate the acTL walk.
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"IDAT");
        bytes.extend_from_slice(&[0u8; 4]); // CRC
        assert_eq!(detect_supported_image_mime_type(&bytes), Some("image/png"));
    }

    #[test]
    fn detects_gif_webp() {
        let gif = b"GIF89a";
        assert_eq!(detect_supported_image_mime_type(gif), Some("image/gif"));
        let mut webp = b"RIFF????WEBP".to_vec();
        // The ???? is 4 placeholder bytes for size — startswith just checks the
        // ascii positions, so this is fine.
        let _ = &mut webp;
        assert_eq!(detect_supported_image_mime_type(&webp), Some("image/webp"));
    }

    #[test]
    fn rejects_empty_and_unknown() {
        assert_eq!(detect_supported_image_mime_type(&[]), None);
        assert_eq!(detect_supported_image_mime_type(b"hello world"), None);
    }

    #[test]
    fn base64_no_newline() {
        let s = encode_base64(b"hello");
        assert!(!s.contains('\n'));
        // base64("hello") = "aGVsbG8="
        assert_eq!(s, "aGVsbG8=");
    }

    #[test]
    fn base64_round_trip_empty() {
        assert_eq!(encode_base64(&[]), "");
    }
}
