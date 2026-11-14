//! Regression specifications; not executed as part of the foundation authoring.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};

fn fixture(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 251) as u8, 91])
    }))
    .write_to(&mut output, format)
    .unwrap();
    output.into_inner()
}

fn validate(bytes: &[u8], mime: &str) -> Result<(), String> {
    validate_rpc_inline_image(&STANDARD.encode(bytes), mime)
}

#[test]
fn real_png_and_jpeg_keep_exact_prepare_inline_image_bytes() {
    for (format, mime) in [
        (ImageFormat::Png, "image/png"),
        (ImageFormat::Jpeg, "image/jpeg"),
    ] {
        let bytes = fixture(format, 9, 7);
        validate(&bytes, mime).unwrap();
        let prepared = prepare_inline_image(&bytes).unwrap();
        assert_eq!(prepared.mime_type, mime);
        assert_eq!(prepared.data, STANDARD.encode(&bytes));
        assert_eq!(STANDARD.decode(&prepared.data).unwrap(), bytes);
        assert!(prepared.hints.is_empty());
    }
}

#[test]
fn strict_base64_and_exact_mime_never_echo_payloads() {
    let png = STANDARD.encode(fixture(ImageFormat::Png, 1, 1));
    for data in [
        "".to_owned(),
        "secret-payload!".to_owned(),
        "Zg".to_owned(),
        "Zh==".to_owned(),
        "Zg==\n".to_owned(),
        "_w==".to_owned(),
        format!("data:image/png;base64,{png}"),
        format!("{png}\n"),
    ] {
        let error = validate_rpc_inline_image(&data, "image/png").unwrap_err();
        assert!(!error.contains("secret-payload"));
        assert!(!error.contains("data:image/png;base64,"));
    }
    for mime in [
        "image/jpg",
        "IMAGE/PNG",
        "image/png;charset=utf-8",
        "image/webp",
        "secret-mime",
    ] {
        let error = validate_rpc_inline_image(&png, mime).unwrap_err();
        assert!(!error.contains("secret-mime"));
    }
    assert!(validate_rpc_inline_image(&png, "image/jpeg").is_err());
    assert!(validate(&fixture(ImageFormat::Jpeg, 1, 1), "image/png").is_err());
}

#[test]
fn per_image_byte_boundary_is_exact_including_base64_padding() {
    let mut png = fixture(ImageFormat::Png, 1, 1);
    // Trailing bytes are preserved by prepare_inline_image, not normalized.
    png.resize(RPC_IMAGE_MAX_BYTES, 0);
    let data = STANDARD.encode(&png);
    assert_eq!(data.len(), RPC_IMAGE_MAX_BASE64_BYTES);
    validate_rpc_inline_image(&data, "image/png").unwrap();
    assert_eq!(prepare_inline_image(&png).unwrap().data, data);
    png.push(0);
    // Same base64 encoded length: only a decoded-length check catches +1.
    assert_eq!(STANDARD.encode(&png).len(), data.len());
    assert!(validate(&png, "image/png").is_err());
    assert!(
        validate_rpc_inline_image(&"A".repeat(RPC_IMAGE_MAX_BASE64_BYTES + 1), "image/png")
            .is_err()
    );
}

#[test]
fn invalid_signatures_and_real_but_truncated_bodies_fail() {
    for (format, mime) in [
        (ImageFormat::Png, "image/png"),
        (ImageFormat::Jpeg, "image/jpeg"),
    ] {
        let bytes = fixture(format, 16, 16);
        // Keep the format signature but omit all meaningful pixel data.
        assert!(validate(&bytes[..16], mime).is_err());
        let mut bad = bytes.clone();
        bad[0] = 0;
        assert!(validate(&bad, mime).is_err());
    }
    assert!(validate(b"\x89PNG\r\n\x1a\n", "image/png").is_err());
    assert!(validate(b"\xff\xd8\xff\xe0", "image/jpeg").is_err());
}

#[test]
fn unsupported_decodable_formats_are_rejected_not_converted() {
    for format in [ImageFormat::Bmp, ImageFormat::Gif] {
        let bytes = fixture(format, 2, 2);
        assert!(validate(&bytes, "image/png").is_err());
        assert!(validate(&bytes, "image/jpeg").is_err());
    }
}

#[test]
fn dimension_boundary_and_skinny_images_are_checked_before_pixels() {
    for (width, height, accepted) in [
        (1, 1, true),
        (2000, 2000, true),
        (2001, 1, false),
        (1, 2001, false),
    ] {
        let png = fixture(ImageFormat::Png, width, height);
        assert!(png.len() <= RPC_IMAGE_MAX_BYTES);
        assert_eq!(validate(&png, "image/png").is_ok(), accepted);
    }
    // Corrupt only IHDR dimensions; neither zero nor huge dimensions may pass.
    let mut png = fixture(ImageFormat::Png, 1, 1);
    png[16..20].copy_from_slice(&0u32.to_be_bytes());
    assert!(validate(&png, "image/png").is_err());
    png[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(validate(&png, "image/png").is_err());
}

fn with_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    // APP1 Exif: little-endian TIFF, one SHORT Orientation IFD entry.
    let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    exif.extend_from_slice(&orientation.to_le_bytes());
    exif.extend_from_slice(&[0; 6]); // inline value padding + next IFD offset
    let mut output = jpeg[..2].to_vec();
    output.extend_from_slice(&[0xff, 0xe1]);
    output.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    output.extend_from_slice(&exif);
    output.extend_from_slice(&jpeg[2..]);
    output
}

#[test]
fn rotated_or_mirrored_jpeg_requires_client_normalization() {
    let jpeg = fixture(ImageFormat::Jpeg, 3, 2);
    validate(&with_orientation(&jpeg, 1), "image/jpeg").unwrap();
    for orientation in 2..=8 {
        let bytes = with_orientation(&jpeg, orientation);
        let error = validate(&bytes, "image/jpeg").unwrap_err();
        assert!(error.contains("normalize it on the client"));
    }
}

#[test]
fn allocation_budget_is_configured_not_a_claim_of_hard_heap_accounting() {
    assert_eq!(RPC_IMAGE_MAX_ALLOC, 67_108_864);
    assert_eq!(RPC_IMAGE_MAX_PIXELS, 4_000_000);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(RPC_IMAGE_MAX_ALLOC);
    limits.reserve(RPC_IMAGE_MAX_ALLOC).unwrap();
    assert!(limits.reserve(1).is_err());
    // image 0.25.6 calls max_alloc non-strict; do not assert a process RSS cap.
}
