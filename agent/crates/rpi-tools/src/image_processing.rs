//! Image processing utilities — resize, convert, and transform images.
//!
//! Mirrors `packages/coding-agent/src/utils/image-*.ts`. Provides cross-platform
//! image processing for the agent's vision capabilities.
//!
//! # Features
//!
//! - **Resize**: Scale images while maintaining aspect ratio
//! - **Convert**: Convert between image formats (PNG, JPEG, WebP)
//! - **EXIF handling**: Strip EXIF data for privacy
//! - **Optimization**: Reduce file size for API transmission
//!
//! # Example
//!
//! ```rust,no_run
//! use rpi_tools::image_processing::{process_image, ImageProcessingOptions, OutputFormat};
//!
//! # fn example(input_bytes: &[u8]) -> Result<(), String> {
//! let options = ImageProcessingOptions {
//!     max_width: Some(1024),
//!     max_height: Some(1024),
//!     format: Some(OutputFormat::Jpeg { quality: 85 }),
//!     strip_exif: true,
//! };
//!
//! let processed = process_image(input_bytes, options)?;
//! # Ok(())
//! # }
//! ```

use image::{DynamicImage, ImageFormat};
use std::io::Cursor;

/// Pi's default inline limits: 2000 pixels per side and 4.5 MiB of base64.
pub const INLINE_IMAGE_MAX_SIDE: u32 = 2000;
pub const INLINE_IMAGE_MAX_BASE64_BYTES: usize = 9 * 1024 * 1024 / 2;

/// Restricted RPC input profile. This is deliberately narrower than tool images.
pub const RPC_IMAGE_MAX_BYTES: usize = 1024 * 1024;
pub const RPC_IMAGE_MAX_BASE64_BYTES: usize = RPC_IMAGE_MAX_BYTES.div_ceil(3) * 4;
pub const RPC_IMAGE_MAX_PIXELS: u64 = 4_000_000;
pub const RPC_IMAGE_MAX_ALLOC: u64 = 64 * 1024 * 1024;

/// Validate without transforming or omitting any image. Call on a blocking worker
/// before accepting a prompt, not via `normalize_image_blocks` (which omits errors).
/// Errors deliberately exclude decoder diagnostics and all client payloads.
///
/// image 0.25.6 enforces strict dimensions, but its `Limits::max_alloc` is a
/// best-effort decoder budget (in particular JPEG does not account all scratch
/// allocations). This is not a process-wide hard memory ceiling.
pub fn validate_rpc_inline_image(data: &str, mime_type: &str) -> Result<(), String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use image::ImageDecoder;

    let format = match mime_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        _ => return Err("image MIME must be image/png or image/jpeg".into()),
    };
    if data.is_empty() || data.len() > RPC_IMAGE_MAX_BASE64_BYTES {
        return Err(format!(
            "image data must contain 1..{} decoded bytes",
            RPC_IMAGE_MAX_BYTES
        ));
    }
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| "image data must be canonical standard base64".to_string())?;
    if bytes.is_empty() || bytes.len() > RPC_IMAGE_MAX_BYTES {
        return Err(format!(
            "image data must contain 1..{} decoded bytes",
            RPC_IMAGE_MAX_BYTES
        ));
    }
    if STANDARD.encode(&bytes) != data {
        return Err("image data must be canonical standard base64".into());
    }
    if crate::image::detect_supported_image_mime_type(&bytes) != Some(mime_type) {
        return Err("image signature does not match the declared MIME".into());
    }

    // Set limits before constructing the decoder: PNG needs the allocation
    // budget during header parsing. ImageReader::into_decoder also calls
    // decoder.set_limits for the locked image 0.25.6 implementation.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(INLINE_IMAGE_MAX_SIDE);
    limits.max_image_height = Some(INLINE_IMAGE_MAX_SIDE);
    limits.max_alloc = Some(RPC_IMAGE_MAX_ALLOC);
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
    reader.limits(limits.clone());
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| "image header is invalid or exceeds decoder limits".to_string())?;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > INLINE_IMAGE_MAX_SIDE
        || height > INLINE_IMAGE_MAX_SIDE
        || u64::from(width) * u64::from(height) > RPC_IMAGE_MAX_PIXELS
    {
        return Err("image dimensions must be 1..2000 per side and at most 4000000 pixels".into());
    }
    // DynamicImage::from_decoder does not itself reserve its output against
    // Limits. Reserve it explicitly before pixel allocation and pass the
    // remaining budget to set_limits, matching ImageReader::decode. The locked
    // PNG decoder cannot update its internal budget here, and JPEG scratch
    // allocations are not accounted (see the public function documentation).
    limits.reserve(decoder.total_bytes()).map_err(|_| {
        format!(
            "image exceeds the {} MiB decode allocation budget",
            RPC_IMAGE_MAX_ALLOC / (1024 * 1024)
        )
    })?;
    decoder
        .set_limits(limits)
        .map_err(|_| "image exceeds decoder limits".to_string())?;
    if decoder
        .orientation()
        .map_err(|_| "image orientation metadata is invalid".to_string())?
        != image::metadata::Orientation::NoTransforms
    {
        return Err(
            "image requires orientation transforms; normalize it on the client first".into(),
        );
    }
    DynamicImage::from_decoder(decoder)
        .map_err(|_| "image body is invalid or exceeds decoder limits".to_string())?;
    Ok(())
}

#[cfg(test)]
#[path = "image_processing_rpc_tests.rs"]
mod rpc_tests;

/// Decode, orient, resize and encode an image for model input. Small supported
/// images retain their original bytes; oversized images fall back to JPEG.
pub fn prepare_inline_image(bytes: &[u8]) -> Result<crate::tools::read::ProcessedImage, String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use image::ImageDecoder;
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    let original = (img.width(), img.height());
    let mime = crate::image::detect_supported_image_mime_type(bytes);
    let base64_len = bytes.len().div_ceil(3) * 4;
    if matches!(
        mime,
        Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
    ) && original.0 <= INLINE_IMAGE_MAX_SIDE
        && original.1 <= INLINE_IMAGE_MAX_SIDE
        && base64_len < INLINE_IMAGE_MAX_BASE64_BYTES
        && orientation == image::metadata::Orientation::NoTransforms
    {
        return Ok(crate::tools::read::ProcessedImage {
            data: STANDARD.encode(bytes),
            mime_type: mime.unwrap().into(),
            hints: vec![],
        });
    }
    img = resize_image(
        img,
        Some(INLINE_IMAGE_MAX_SIDE),
        Some(INLINE_IMAGE_MAX_SIDE),
    );
    for _ in 0..8 {
        let png = encode_image(&img, Some(OutputFormat::Png))?;
        let mut candidate = (png, "image/png");
        if candidate.0.len().div_ceil(3) * 4 >= INLINE_IMAGE_MAX_BASE64_BYTES {
            // JPEG cannot encode alpha; convert explicitly before encoding.
            let mut rgb = img.to_rgb8();
            let rgba = img.to_rgba8();
            for (target, source) in rgb.pixels_mut().zip(rgba.pixels()) {
                let alpha = source[3] as u16;
                for channel in 0..3 {
                    target[channel] =
                        ((source[channel] as u16 * alpha + 255 * (255 - alpha)) / 255) as u8;
                }
            }
            let rgb = DynamicImage::ImageRgb8(rgb);
            for quality in [85, 70, 50] {
                let jpeg = encode_image(&rgb, Some(OutputFormat::Jpeg { quality }))?;
                if jpeg.len() < candidate.0.len() {
                    candidate = (jpeg, "image/jpeg");
                }
                if candidate.0.len().div_ceil(3) * 4 < INLINE_IMAGE_MAX_BASE64_BYTES {
                    break;
                }
            }
        }
        if candidate.0.len().div_ceil(3) * 4 < INLINE_IMAGE_MAX_BASE64_BYTES {
            let mut hints = vec![];
            if let Some(mime) = mime {
                if mime != candidate.1 {
                    hints.push(format!("[Image converted from {mime} to {}.]", candidate.1));
                }
            }
            if original != (img.width(), img.height()) {
                hints.push(format!(
                    "[Image resized from {}x{} to {}x{}.]",
                    original.0,
                    original.1,
                    img.width(),
                    img.height()
                ));
            }
            return Ok(crate::tools::read::ProcessedImage {
                data: STANDARD.encode(candidate.0),
                mime_type: candidate.1.into(),
                hints,
            });
        }
        let bounds = ((img.width() * 3 / 4).max(1), (img.height() * 3 / 4).max(1));
        img = resize_image(img, Some(bounds.0), Some(bounds.1));
    }
    Err("Image could not be resized below the inline image size limit".into())
}

/// Shared processor for the built-in read tool.
pub struct InlineImageProcessor;

/// Normalize image blocks while retaining order and surfacing omissions as
/// text, rather than sending an invalid image to a provider.
pub fn normalize_image_blocks(blocks: &mut Vec<rpi_ai::types::Content>) {
    use rpi_ai::types::Content;
    let mut output = Vec::with_capacity(blocks.len());
    for block in std::mem::take(blocks) {
        if let Content::Image(mut img) = block {
            use base64::{engine::general_purpose::STANDARD, Engine};
            match STANDARD
                .decode(&*img.data)
                .map_err(|e| e.to_string())
                .and_then(|bytes| prepare_inline_image(&bytes))
            {
                Ok(processed) => {
                    img.data = processed.data.into();
                    img.mime_type = processed.mime_type;
                    output.push(Content::Image(img));
                    output.extend(processed.hints.into_iter().map(Content::text));
                }
                Err(error) => output.push(Content::text(format!("[Image omitted: {error}.]"))),
            }
        } else {
            output.push(block);
        }
    }
    *blocks = output;
}

impl crate::tools::read::ReadImageProcessor for InlineImageProcessor {
    fn process(
        &self,
        bytes: &[u8],
        _mime_type: &str,
        auto_resize: bool,
    ) -> Result<crate::tools::read::ProcessedImage, String> {
        if auto_resize {
            return prepare_inline_image(bytes);
        }
        use base64::{engine::general_purpose::STANDARD, Engine};
        let mime = crate::image::detect_supported_image_mime_type(bytes);
        let (bytes, mime) = if matches!(
            mime,
            Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
        ) {
            (bytes.to_vec(), mime.unwrap())
        } else {
            (
                process_image(bytes, ImageProcessingOptions::default())?,
                "image/png",
            )
        };
        Ok(crate::tools::read::ProcessedImage {
            data: STANDARD.encode(bytes),
            mime_type: mime.into(),
            hints: vec![],
        })
    }
}

/// Output format for processed images.
#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    /// PNG format (lossless)
    Png,
    /// JPEG format with quality (1-100)
    Jpeg { quality: u8 },
    /// WebP format with quality (1-100)
    WebP { quality: u8 },
}

/// Options for image processing.
#[derive(Debug, Clone)]
pub struct ImageProcessingOptions {
    /// Maximum width in pixels. If set, image is scaled down to fit.
    pub max_width: Option<u32>,
    /// Maximum height in pixels. If set, image is scaled down to fit.
    pub max_height: Option<u32>,
    /// Output format. If `None`, preserves the input format.
    pub format: Option<OutputFormat>,
    /// Strip EXIF metadata (privacy). Default: `true`.
    pub strip_exif: bool,
}

impl Default for ImageProcessingOptions {
    fn default() -> Self {
        Self {
            max_width: None,
            max_height: None,
            format: None,
            strip_exif: true,
        }
    }
}

/// Process an image according to the given options.
///
/// # Arguments
///
/// * `input` - Raw image bytes (PNG, JPEG, WebP, etc.)
/// * `options` - Processing options (resize, format, EXIF stripping)
///
/// # Returns
///
/// * `Ok(Vec<u8>)` - Processed image bytes
/// * `Err(String)` - Processing failed
///
/// # Example
///
/// ```rust,no_run
/// use rpi_tools::image_processing::{process_image, ImageProcessingOptions, OutputFormat};
///
/// # fn example(raw_bytes: &[u8]) -> Result<(), String> {
/// let options = ImageProcessingOptions {
///     max_width: Some(800),
///     max_height: Some(600),
///     format: Some(OutputFormat::Jpeg { quality: 85 }),
///     strip_exif: true,
/// };
///
/// let processed = process_image(raw_bytes, options)?;
/// # Ok(())
/// # }
/// ```
pub fn process_image(input: &[u8], options: ImageProcessingOptions) -> Result<Vec<u8>, String> {
    // Load the image
    let mut img =
        image::load_from_memory(input).map_err(|e| format!("Failed to load image: {}", e))?;

    // Resize if needed
    if options.max_width.is_some() || options.max_height.is_some() {
        img = resize_image(img, options.max_width, options.max_height);
    }

    // Encode to the desired format
    let output_bytes = encode_image(&img, options.format)?;

    Ok(output_bytes)
}

/// Resize an image to fit within the given bounds while maintaining aspect ratio.
fn resize_image(
    img: DynamicImage,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> DynamicImage {
    let (orig_width, orig_height) = (img.width(), img.height());

    // Calculate the scaling factor
    let width_scale = max_width
        .map(|mw| mw as f64 / orig_width as f64)
        .unwrap_or(1.0);
    let height_scale = max_height
        .map(|mh| mh as f64 / orig_height as f64)
        .unwrap_or(1.0);

    let scale = width_scale.min(height_scale).min(1.0); // Never upscale

    if scale >= 1.0 {
        // No need to resize
        return img;
    }

    // A skinny image can round one side to zero; encoders reject that.
    let new_width = ((orig_width as f64 * scale).round() as u32).max(1);
    let new_height = ((orig_height as f64 * scale).round() as u32).max(1);

    resize_exact(&img, new_width, new_height)
}

/// Lanczos3 to exactly `width`×`height`. 8-bit images go through
/// `fast_image_resize` (SIMD, alpha-aware); other pixel types keep `image`'s
/// resize.
pub(crate) fn resize_exact(img: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    use fast_image_resize::{images::Image, FilterType, ResizeAlg, ResizeOptions, Resizer};
    let fast = || {
        let pixel_type = match img {
            DynamicImage::ImageLuma8(_)
            | DynamicImage::ImageLumaA8(_)
            | DynamicImage::ImageRgb8(_)
            | DynamicImage::ImageRgba8(_) => fast_image_resize::IntoImageView::pixel_type(img)?,
            _ => return None,
        };
        let mut dst = Image::new(width, height, pixel_type);
        let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
        Resizer::new().resize(img, &mut dst, &options).ok()?;
        let buf = dst.into_vec();
        match img {
            DynamicImage::ImageLuma8(_) => {
                image::GrayImage::from_raw(width, height, buf).map(DynamicImage::ImageLuma8)
            }
            DynamicImage::ImageLumaA8(_) => {
                image::GrayAlphaImage::from_raw(width, height, buf).map(DynamicImage::ImageLumaA8)
            }
            DynamicImage::ImageRgb8(_) => {
                image::RgbImage::from_raw(width, height, buf).map(DynamicImage::ImageRgb8)
            }
            _ => image::RgbaImage::from_raw(width, height, buf).map(DynamicImage::ImageRgba8),
        }
    };
    fast().unwrap_or_else(|| img.resize_exact(width, height, image::imageops::FilterType::Lanczos3))
}

/// Encode an image to the specified format.
fn encode_image(img: &DynamicImage, format: Option<OutputFormat>) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    let mut cursor = Cursor::new(&mut buffer);

    match format {
        Some(OutputFormat::Png) => {
            img.write_to(&mut cursor, ImageFormat::Png)
                .map_err(|e| format!("Failed to encode PNG: {}", e))?;
        }
        Some(OutputFormat::Jpeg { quality }) => {
            // For JPEG with quality, we need to use the encoder directly
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality);
            img.write_with_encoder(encoder)
                .map_err(|e| format!("Failed to encode JPEG: {}", e))?;
        }
        Some(OutputFormat::WebP { quality }) => {
            // WebP encoding requires the webp feature
            #[cfg(feature = "webp")]
            {
                let encoder = image::codecs::webp::WebPEncoder::new_with_quality(
                    &mut cursor,
                    image::codecs::webp::WebPQuality::lossy(quality),
                );
                img.write_with_encoder(encoder)
                    .map_err(|e| format!("Failed to encode WebP: {}", e))?;
            }
            #[cfg(not(feature = "webp"))]
            {
                let _ = quality;
                return Err("WebP encoding requires the 'webp' feature".to_string());
            }
        }
        None => {
            // Default to PNG
            img.write_to(&mut cursor, ImageFormat::Png)
                .map_err(|e| format!("Failed to encode image: {}", e))?;
        }
    }

    Ok(buffer)
}

/// Detect the MIME type of an image from its bytes.
///
/// # Arguments
///
/// * `data` - Raw image bytes
///
/// # Returns
///
/// * `Some(&str)` - MIME type (e.g., "image/png", "image/jpeg")
/// * `None` - Unknown or unsupported format
pub fn detect_image_mime(data: &[u8]) -> Option<&'static str> {
    // Check magic bytes
    if data.len() < 8 {
        return None;
    }

    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }

    // JPEG: FF D8 FF
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }

    // GIF: 47 49 46 38
    if data.starts_with(&[0x47, 0x49, 0x46, 0x38]) {
        return Some("image/gif");
    }

    // WebP: 52 49 46 46 ... 57 45 42 50
    if data.len() >= 12
        && data.starts_with(&[0x52, 0x49, 0x46, 0x46])
        && data[8..12].starts_with(&[0x57, 0x45, 0x42, 0x50])
    {
        return Some("image/webp");
    }

    // BMP: 42 4D
    if data.starts_with(&[0x42, 0x4D]) {
        return Some("image/bmp");
    }

    None
}

/// Get image dimensions without fully decoding the image.
///
/// # Arguments
///
/// * `data` - Raw image bytes
///
/// # Returns
///
/// * `Ok((width, height))` - Image dimensions in pixels
/// * `Err(String)` - Failed to read dimensions
pub fn get_image_dimensions(data: &[u8]) -> Result<(u32, u32), String> {
    let img = image::load_from_memory(data).map_err(|e| format!("Failed to load image: {}", e))?;
    Ok((img.width(), img.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_exact_matches_image_lanczos3_closely() {
        let source = DynamicImage::ImageRgb8(image::RgbImage::from_fn(400, 300, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x * y) % 256) as u8])
        }));
        let fast = resize_exact(&source, 173, 97).to_rgb8();
        let slow = source
            .resize_exact(173, 97, image::imageops::FilterType::Lanczos3)
            .to_rgb8();
        let worst = fast
            .as_raw()
            .iter()
            .zip(slow.as_raw())
            .map(|(a, b)| a.abs_diff(*b))
            .max();
        assert!(worst <= Some(8), "worst channel difference {worst:?}");
        let gray16 = resize_exact(
            &DynamicImage::ImageLuma16(image::ImageBuffer::new(40, 30)),
            20,
            15,
        );
        assert_eq!(
            (gray16.width(), gray16.height()),
            (20, 15),
            "other pixel types fall back"
        );
    }

    #[test]
    fn inline_images_keep_small_bytes_and_resize_large_images() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let small = encode_image(&DynamicImage::new_rgba8(8, 8), Some(OutputFormat::Png)).unwrap();
        let processed = prepare_inline_image(&small).unwrap();
        assert_eq!(STANDARD.decode(processed.data).unwrap(), small);
        let large = encode_image(
            &DynamicImage::new_rgba8(2400, 1200),
            Some(OutputFormat::Png),
        )
        .unwrap();
        let processed = prepare_inline_image(&large).unwrap();
        assert!(processed.data.len() < INLINE_IMAGE_MAX_BASE64_BYTES);
        let resized = image::load_from_memory(&STANDARD.decode(processed.data).unwrap()).unwrap();
        assert_eq!((resized.width(), resized.height()), (2000, 1000));
        assert!(processed.hints[0].contains("2400x1200 to 2000x1000"));
    }

    #[test]
    fn inline_skinny_images_keep_a_nonzero_side() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        for (width, height, expected) in [(1, 10_000, (1, 2000)), (10_000, 1, (2000, 1))] {
            let png = encode_image(
                &DynamicImage::new_rgb8(width, height),
                Some(OutputFormat::Png),
            )
            .unwrap();
            let processed = prepare_inline_image(&png).unwrap();
            let resized =
                image::load_from_memory(&STANDARD.decode(processed.data).unwrap()).unwrap();
            assert_eq!((resized.width(), resized.height()), expected);
        }
    }

    #[test]
    fn inline_bmp_is_converted_and_invalid_images_are_reported() {
        let mut bmp = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(4, 4)
            .write_to(&mut bmp, ImageFormat::Bmp)
            .unwrap();
        let processed = prepare_inline_image(&bmp.into_inner()).unwrap();
        assert_eq!(processed.mime_type, "image/png");
        let mut blocks = vec![rpi_ai::types::Content::Image(rpi_ai::types::ImageContent {
            kind: rpi_ai::types::ImageContentType,
            data: "invalid".into(),
            mime_type: "image/png".into(),
        })];
        normalize_image_blocks(&mut blocks);
        assert!(
            matches!(&blocks[0], rpi_ai::types::Content::Text(text) if text.text.contains("Image omitted"))
        );
    }

    #[test]
    fn inline_images_reduce_encoded_payload_even_below_dimension_limit() {
        let mut rgba = image::RgbaImage::new(1200, 1200);
        let mut seed = 1u32;
        for pixel in rgba.pixels_mut() {
            for channel in &mut pixel.0 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *channel = seed as u8;
            }
        }
        let png = encode_image(&DynamicImage::ImageRgba8(rgba), Some(OutputFormat::Png)).unwrap();
        assert!(png.len().div_ceil(3) * 4 > INLINE_IMAGE_MAX_BASE64_BYTES);
        let processed = prepare_inline_image(&png).unwrap();
        assert!(processed.data.len() < INLINE_IMAGE_MAX_BASE64_BYTES);
        assert_eq!(processed.mime_type, "image/jpeg");
    }

    #[test]
    fn test_detect_image_mime_png() {
        let png_header = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(detect_image_mime(&png_header), Some("image/png"));
    }

    #[test]
    fn test_detect_image_mime_jpeg() {
        let jpeg_header = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        assert_eq!(detect_image_mime(&jpeg_header), Some("image/jpeg"));
    }

    #[test]
    fn test_detect_image_mime_unknown() {
        let unknown = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        assert_eq!(detect_image_mime(&unknown), None);
    }

    #[test]
    fn test_detect_image_mime_too_short() {
        let short = vec![0x89, 0x50];
        assert_eq!(detect_image_mime(&short), None);
    }
}
