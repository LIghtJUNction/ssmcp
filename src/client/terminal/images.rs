//! Bounded native previews; unsupported terminals retain clean attachment metadata.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use serde_json::Value;
use std::{io::Cursor, sync::Arc};

mod kitty;
pub(crate) use kitty::{CACHE_BYTES, CellSize, Graphics, Native, Placement, Renderer};

const MAX_DIMENSION: u32 = 4096;
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Preview {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) native: Option<Arc<Native>>,
}

/// Decode an already-authorized MCP image block once, outside the drawing loop.
/// Source paths are never opened here. The full decoded buffer is temporary;
/// only the optional native PNG cache survives decoding; fallback cards keep metadata.
pub(crate) fn decode(block: &Value, native: bool) -> Result<Preview, String> {
    crate::output::validate_image_content(block)?;
    let format = match block["mimeType"].as_str() {
        Some("image/png") => ImageFormat::Png,
        Some("image/jpeg") => ImageFormat::Jpeg,
        Some("image/gif") => ImageFormat::Gif,
        Some("image/webp") => ImageFormat::WebP,
        _ => return Err("unsupported preview image format".into()),
    };
    let bytes = STANDARD
        .decode(block["data"].as_str().ok_or("image data must be base64")?)
        .map_err(|_| "image data must be canonical base64")?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    // Decoder allocation accounting is best effort. Dimensions and the exact
    // output-buffer size are checked separately before pixels are allocated.
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|error| format!("image preview: {error}"))?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err("image preview dimensions must be 1..=4096 pixels per side".into());
    }
    if decoder.total_bytes() > MAX_DECODED_BYTES {
        return Err("image preview decoded pixels exceed 64 MiB; use a smaller image".into());
    }
    let decoded =
        DynamicImage::from_decoder(decoder).map_err(|error| format!("image preview: {error}"))?;
    let native = native.then(|| Native::prepare(&decoded)).flatten();
    Ok(Preview {
        width,
        height,
        native,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbaImage;
    use serde_json::json;

    fn encoded(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn block(mime: &str, bytes: &[u8]) -> Value {
        json!({"type":"image","mimeType":mime,"data":STANDARD.encode(bytes)})
    }

    #[test]
    fn decodes_each_supported_format_with_bounded_preview_storage() {
        let pixels = RgbaImage::from_pixel(16, 8, image::Rgba([30, 120, 210, 255]));
        for (mime, format) in [
            ("image/png", ImageFormat::Png),
            ("image/jpeg", ImageFormat::Jpeg),
            ("image/gif", ImageFormat::Gif),
            ("image/webp", ImageFormat::WebP),
        ] {
            let image = DynamicImage::ImageRgba8(pixels.clone());
            let image = if format == ImageFormat::Jpeg {
                DynamicImage::ImageRgb8(image.into_rgb8())
            } else {
                image
            };
            let bytes = encoded(image, format);
            let preview = decode(&block(mime, &bytes), true).unwrap();
            assert_eq!((preview.width, preview.height), (16, 8), "{mime}");
            let native = preview.native.as_ref().unwrap();
            assert_eq!((native.width, native.height), (16, 8));
            assert!(native.bytes() < 4096);
        }
    }

    #[test]
    fn unsupported_terminal_retains_source_dimensions_without_native_pixels() {
        let bytes = encoded(
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(
                320,
                240,
                image::Rgba([30, 120, 210, 255]),
            )),
            ImageFormat::Png,
        );
        let preview = decode(&block("image/png", &bytes), false).unwrap();
        assert_eq!((preview.width, preview.height), (320, 240));
        assert!(preview.native.is_none());
    }

    fn png_header(mut bytes: Vec<u8>, width: u32, height: u32, depth: u8) -> Vec<u8> {
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        bytes[24] = depth;
        bytes[25] = 6; // RGBA
        let mut crc = u32::MAX;
        for &byte in &bytes[12..29] {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb88320
                } else {
                    crc >> 1
                };
            }
        }
        bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
        bytes
    }

    #[test]
    fn malformed_and_oversized_images_fail_before_full_pixel_allocation() {
        assert!(decode(&block("image/png", b"\x89PNG\r\n\x1a\n"), false).is_err());
        assert!(decode(&block("image/png", b"<svg/>"), false).is_err());
        let mut oversized = vec![0; crate::output::MAX_IMAGE_BYTES + 1];
        oversized[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(decode(&block("image/png", &oversized), false).is_err());
        let png = encoded(
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))),
            ImageFormat::Png,
        );
        // These small valid-CRC headers advertise large outputs. Rejection
        // must happen before attempting to decode the tiny IDAT as that image.
        let wide = png_header(png.clone(), MAX_DIMENSION + 1, 1, 8);
        let error = decode(&block("image/png", &wide), false).err().unwrap();
        assert!(
            error.to_lowercase().contains("limit") || error.contains("4096"),
            "{error}"
        );
        let large_pixels = png_header(png, MAX_DIMENSION, MAX_DIMENSION, 16);
        let error = decode(&block("image/png", &large_pixels), false)
            .err()
            .unwrap();
        assert!(
            error.to_lowercase().contains("limit") || error.contains("64 MiB"),
            "{error}"
        );
    }
}
