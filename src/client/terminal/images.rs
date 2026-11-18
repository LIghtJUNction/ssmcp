//! Bounded in-memory thumbnails rendered with ordinary true-color terminal cells.
use crate::client::ui::Ui;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage};
use serde_json::Value;
use std::{fmt::Write as _, io::Cursor};

const MAX_DIMENSION: u32 = 4096;
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const THUMBNAIL_PIXELS: u32 = 128;

#[derive(Clone)]
pub(crate) struct Preview {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pixels: RgbaImage,
}

/// Decode an already-authorized MCP image block once, outside the drawing loop.
/// Source paths are never opened here. Keep only the small thumbnail in UI state.
pub(crate) fn decode(block: &Value) -> Result<Preview, String> {
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
    let pixels = decoded
        .thumbnail(THUMBNAIL_PIXELS.min(width), THUMBNAIL_PIXELS.min(height))
        .into_rgba8();
    Ok(Preview {
        width,
        height,
        pixels,
    })
}

impl Preview {
    /// Fit within the requested cell rectangle; two vertical pixels share ▀.
    /// Rows contain only the image cells and an SGR reset. The caller owns
    /// surrounding margins and padding. At most 128 columns × 64 rows are used.
    pub(crate) fn render(&self, columns: usize, max_rows: usize, ui: &Ui) -> Vec<String> {
        if columns == 0 || max_rows == 0 {
            return Vec::new();
        }
        let width = (columns.min(THUMBNAIL_PIXELS as usize) as u32).min(self.pixels.width());
        let height =
            (max_rows.min(THUMBNAIL_PIXELS as usize / 2) as u32 * 2).min(self.pixels.height());
        let pixels = image::imageops::thumbnail(&self.pixels, width, height);
        let background = background(ui);
        let mut rows = Vec::with_capacity(pixels.height().div_ceil(2) as usize);
        for y in (0..pixels.height()).step_by(2) {
            let mut row = String::new();
            let mut previous = None;
            for x in 0..pixels.width() {
                let upper = composite(pixels.get_pixel(x, y).0, background);
                let lower = if y + 1 < pixels.height() {
                    composite(pixels.get_pixel(x, y + 1).0, background)
                } else {
                    background
                };
                if ui.color {
                    if previous != Some((upper, lower)) {
                        let _ = write!(
                            row,
                            "\x1b[38;2;{};{};{};48;2;{};{};{}m",
                            upper[0], upper[1], upper[2], lower[0], lower[1], lower[2]
                        );
                        previous = Some((upper, lower));
                    }
                    row.push('▀');
                } else {
                    const RAMP: &[u8] = b"@%#*+=-:. ";
                    let gray = (luma(upper) + luma(lower)) / 2;
                    row.push(RAMP[gray as usize * (RAMP.len() - 1) / 255] as char);
                }
            }
            if ui.color {
                row.push_str("\x1b[0m");
            }
            rows.push(row);
        }
        rows
    }
}

fn background(ui: &Ui) -> [u8; 3] {
    if !ui.color {
        return [255; 3];
    }
    let code = ui.background();
    let mut rgb = code.strip_prefix("48;2;").unwrap_or("").split(';');
    match (
        rgb.next().and_then(|n| n.parse().ok()),
        rgb.next().and_then(|n| n.parse().ok()),
        rgb.next().and_then(|n| n.parse().ok()),
    ) {
        (Some(r), Some(g), Some(b)) => [r, g, b],
        _ => [32, 33, 36],
    }
}

fn composite(pixel: [u8; 4], background: [u8; 3]) -> [u8; 3] {
    let alpha = u32::from(pixel[3]);
    std::array::from_fn(|i| {
        ((u32::from(pixel[i]) * alpha + u32::from(background[i]) * (255 - alpha) + 127) / 255) as u8
    })
}

fn luma(rgb: [u8; 3]) -> u32 {
    (u32::from(rgb[0]) * 2126 + u32::from(rgb[1]) * 7152 + u32::from(rgb[2]) * 722) / 10000
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn decodes_each_supported_format_and_retains_only_a_thumbnail() {
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
            let preview = decode(&block(mime, &bytes)).unwrap();
            assert_eq!((preview.width, preview.height), (16, 8), "{mime}");
            assert!(preview.pixels.width() <= THUMBNAIL_PIXELS);
            assert!(preview.pixels.height() <= THUMBNAIL_PIXELS);
            assert!(
                preview.pixels.as_raw().len() <= (THUMBNAIL_PIXELS * THUMBNAIL_PIXELS * 4) as usize
            );
            assert!(!preview.render(16, 4, &Ui::default()).is_empty());
        }
    }

    #[test]
    fn true_color_half_blocks_preserve_upper_and_lower_pixels() {
        let pixels = RgbaImage::from_raw(1, 2, vec![255, 0, 0, 255, 0, 0, 255, 255]).unwrap();
        let preview = Preview {
            width: 1,
            height: 2,
            pixels,
        };
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        let rows = preview.render(1, 1, &ui);
        assert_eq!(rows.len(), 1);
        let dimension = std::num::NonZeroU16::new(2).unwrap();
        let mut parser = vt100::Parser::new(dimension, dimension, 0);
        parser.process(rows[0].as_bytes());
        let cell = parser.screen().cell(0, 0).unwrap();
        assert_eq!(cell.contents(), "▀");
        assert_eq!(cell.fgcolor(), vt100::Color::Rgb(255, 0, 0));
        assert_eq!(cell.bgcolor(), vt100::Color::Rgb(0, 0, 255));
        assert!(!parser.screen().cell(0, 1).unwrap().has_contents());
        assert!(rows[0].ends_with("\x1b[0m"));
        assert_eq!(composite([255, 0, 0, 0], [10, 20, 30]), [10, 20, 30]);
        assert_eq!(composite([255, 0, 0, 128], [0, 0, 255]), [128, 0, 127]);
    }

    #[test]
    fn decoding_and_rendering_tiny_images_never_upscale_pixels() {
        let pixels =
            image::RgbImage::from_raw(2, 2, vec![255, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255])
                .unwrap();
        let bytes = encoded(DynamicImage::ImageRgb8(pixels), ImageFormat::Png);
        let preview = decode(&block("image/png", &bytes)).unwrap();
        assert_eq!((preview.pixels.width(), preview.pixels.height()), (2, 2));
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        let rows = preview.render(120, 30, &ui);
        assert_eq!(rows.len(), 1);
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU16::new(128).unwrap(),
            0,
        );
        parser.process(rows[0].as_bytes());
        for column in 0..2 {
            let cell = parser.screen().cell(0, column).unwrap();
            assert_eq!(cell.contents(), "▀");
            assert_eq!(cell.fgcolor(), vt100::Color::Rgb(255, 0, 0));
            assert_eq!(cell.bgcolor(), vt100::Color::Rgb(0, 0, 255));
        }
        assert!(!parser.screen().cell(0, 2).unwrap().has_contents());
    }

    #[test]
    fn narrow_and_monochrome_previews_fit_without_terminal_commands() {
        let preview = Preview {
            width: 128,
            height: 128,
            pixels: RgbaImage::from_pixel(128, 128, image::Rgba([0, 0, 0, 255])),
        };
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        assert!(preview.render(0, 10, &ui).is_empty());
        assert!(preview.render(10, 0, &ui).is_empty());
        let rows = preview.render(3, 2, &ui);
        assert!(!rows.is_empty() && rows.len() <= 2);
        assert!(
            rows.iter()
                .all(|row| row.len() <= 3 && row.is_ascii() && !row.contains('\x1b'))
        );
        let large = preview.render(usize::MAX, usize::MAX, &ui);
        assert!(large.len() <= 64);
        assert!(large.iter().all(|row| row.len() <= 128));
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
        assert!(decode(&block("image/png", b"\x89PNG\r\n\x1a\n")).is_err());
        assert!(decode(&block("image/png", b"<svg/>")).is_err());
        let mut oversized = vec![0; crate::output::MAX_IMAGE_BYTES + 1];
        oversized[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(decode(&block("image/png", &oversized)).is_err());
        let png = encoded(
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))),
            ImageFormat::Png,
        );
        // These small valid-CRC headers advertise large outputs. Rejection
        // must happen before attempting to decode the tiny IDAT as that image.
        let wide = png_header(png.clone(), MAX_DIMENSION + 1, 1, 8);
        let error = decode(&block("image/png", &wide)).err().unwrap();
        assert!(
            error.to_lowercase().contains("limit") || error.contains("4096"),
            "{error}"
        );
        let large_pixels = png_header(png, MAX_DIMENSION, MAX_DIMENSION, 16);
        let error = decode(&block("image/png", &large_pixels)).err().unwrap();
        assert!(
            error.to_lowercase().contains("limit") || error.contains("64 MiB"),
            "{error}"
        );
    }
}
