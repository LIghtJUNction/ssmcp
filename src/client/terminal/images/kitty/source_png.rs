//! A narrow, allocation-free container check for already-decoded source PNGs.
//! See https://www.w3.org/TR/png-3/ for chunk ordering, CRC and color types.

pub(super) fn compatible(bytes: &[u8], width: u32, height: u32) -> bool {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || width == 0
        || height == 0
        || width > super::super::MAX_DIMENSION
        || height > super::super::MAX_DIMENSION
    {
        return false;
    }
    let mut offset = 8usize;
    let mut image_data = false;
    let mut data_ended = false;
    let mut physical = false;
    while bytes.len().saturating_sub(offset) >= 12 {
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let Some(end) = offset
            .checked_add(12)
            .and_then(|start| start.checked_add(length))
        else {
            return false;
        };
        if end > bytes.len() {
            return false;
        }
        let kind = &bytes[offset + 4..offset + 8];
        let payload = &bytes[offset + 8..end - 4];
        let crc = u32::from_be_bytes(bytes[end - 4..end].try_into().unwrap());
        if crc32fast::hash(&bytes[offset + 4..end - 4]) != crc {
            return false;
        }
        if offset == 8 {
            if kind != b"IHDR"
                || length != 13
                || payload[..4] != width.to_be_bytes()
                || payload[4..8] != height.to_be_bytes()
                || payload[8] != 8
                || !matches!(payload[9], 2 | 6)
                || payload[10..] != [0, 0, 0]
            {
                return false;
            }
        } else {
            match kind {
                b"IDAT" if !data_ended => image_data = true,
                b"IEND" => return image_data && length == 0 && end == bytes.len(),
                b"pHYs" if !image_data && !physical && length == 9 && payload[8] <= 1 => {
                    physical = true;
                }
                b"tEXt" => {
                    // Uncompressed comments cannot affect decoded samples. Keep
                    // the keyword framing valid even if a decoder ignored it.
                    if !payload
                        .iter()
                        .position(|byte| *byte == 0)
                        .is_some_and(|size| (1..=79).contains(&size))
                    {
                        return false;
                    }
                    data_ended |= image_data;
                }
                _ => return false,
            }
        }
        offset = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
    use std::io::Cursor;

    fn png(alpha: bool) -> Vec<u8> {
        let image = if alpha {
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(
                32,
                24,
                image::Rgba([15, 90, 210, 128]),
            ))
        } else {
            DynamicImage::ImageRgb8(RgbImage::from_pixel(32, 24, image::Rgb([15, 90, 210])))
        };
        let mut result = Cursor::new(Vec::new());
        image.write_to(&mut result, ImageFormat::Png).unwrap();
        result.into_inner()
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(data);
        let crc = crc32fast::hash(&chunk[4..]);
        chunk.extend_from_slice(&crc.to_be_bytes());
        chunk
    }

    fn before_data(mut png: Vec<u8>, kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        png.splice(33..33, chunk(kind, data));
        png
    }

    fn preview(bytes: &[u8]) -> super::super::super::Preview {
        super::super::super::decode(&serde_json::json!({"type":"image", "mimeType":"image/png", "data":STANDARD.encode(bytes)}), true).unwrap()
    }

    #[test]
    fn ordinary_rgb_and_rgba_reuse_exact_validated_source_bytes() {
        for alpha in [false, true] {
            let source = png(alpha);
            assert!(compatible(&source, 32, 24));
            let native = preview(&source).native.unwrap();
            let saved = STANDARD.decode(&native.encoded).unwrap();
            assert_eq!(saved, source);
            assert_eq!((native.width, native.height), (32, 24));
        }
    }

    #[test]
    fn physical_resolution_and_uncompressed_comments_keep_the_same_pixels() {
        let source = before_data(png(true), b"pHYs", &[0, 0, 11, 19, 0, 0, 11, 19, 1]);
        let source = before_data(source, b"tEXt", b"Comment\0source PNG preserved");
        let saved = STANDARD
            .decode(&preview(&source).native.unwrap().encoded)
            .unwrap();
        assert_eq!(saved, source);
        assert_eq!(
            image::load_from_memory(&saved)
                .unwrap()
                .to_rgba8()
                .get_pixel(0, 0)
                .0,
            [15, 90, 210, 128]
        );
    }

    #[test]
    fn unfamiliar_transform_and_animation_chunks_use_the_conversion_path() {
        for (kind, data) in [
            (b"sRGB", b"\0".as_slice()),
            (b"gAMA", &[0, 0, 177, 143]),
            (b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]),
            (b"zTXt", b"Comment\0\0compressed"),
        ] {
            let source = before_data(png(false), kind, data);
            assert!(!compatible(&source, 32, 24), "{kind:?}");
        }
        let source = before_data(png(false), b"sRGB", b"\0");
        let saved = STANDARD
            .decode(&preview(&source).native.unwrap().encoded)
            .unwrap();
        assert_ne!(saved, source);
        assert_eq!(
            image::load_from_memory(&saved).unwrap().to_rgba8(),
            image::load_from_memory(&source).unwrap().to_rgba8()
        );
    }

    #[test]
    fn malformed_crc_lengths_ordering_and_trailing_bytes_never_pass_through() {
        let original = png(true);
        for cut in 0..original.len() {
            assert!(!compatible(&original[..cut], 32, 24));
        }
        for index in [8, 16, 29, original.len() - 1] {
            let mut broken = original.clone();
            broken[index] ^= 0x80;
            assert!(!compatible(&broken, 32, 24));
        }
        let mut trailing = original.clone();
        trailing.push(0);
        assert!(!compatible(&trailing, 32, 24));
        assert!(!compatible(&original, 31, 24));
        assert!(!compatible(
            &before_data(original.clone(), b"tEXt", b"\0empty keyword"),
            32,
            24
        ));
        assert!(!compatible(
            &before_data(original.clone(), b"pHYs", &[0; 8]),
            32,
            24
        ));
        let mut second_header = original.clone();
        second_header.splice(33..33, original[8..33].iter().copied());
        assert!(!compatible(&second_header, 32, 24));
    }

    #[test]
    fn source_reuse_still_requires_full_pixel_decoding() {
        let mut source = png(true);
        let length = u32::from_be_bytes(source[33..37].try_into().unwrap()) as usize;
        assert_eq!(&source[37..41], b"IDAT");
        source[41] ^= 0xff;
        let crc = crc32fast::hash(&source[37..41 + length]);
        source[41 + length..45 + length].copy_from_slice(&crc.to_be_bytes());
        assert!(
            compatible(&source, 32, 24),
            "container integrity alone is insufficient"
        );
        let result = super::super::super::decode(
            &serde_json::json!({"type":"image", "mimeType":"image/png", "data":STANDARD.encode(source)}),
            true,
        );
        assert!(
            result.is_err(),
            "invalid compressed pixels must never reach the terminal"
        );
    }

    #[test]
    #[ignore = "release-only microbenchmark; supply SSMCP_PNG_BENCH_INPUTS as a JSON path list"]
    fn measure_preparation_after_common_pixel_validation() {
        use std::{hint::black_box, time::Instant};
        let inputs: Vec<String> =
            serde_json::from_str(&std::env::var("SSMCP_PNG_BENCH_INPUTS").expect("fixture paths"))
                .unwrap();
        assert!(!inputs.is_empty() && inputs.len() <= 8);
        for input in inputs {
            let bytes = std::fs::read(&input).unwrap();
            let encoded = STANDARD.encode(&bytes);
            let decoded = image::load_from_memory(&bytes).unwrap();
            let mut legacy = Vec::new();
            let mut reused = Vec::new();
            for trial in 0..10 {
                for reuse in if trial % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                } {
                    let started = Instant::now();
                    let native = if reuse {
                        super::super::Native::from_png(&decoded, &bytes, &encoded)
                    } else {
                        super::super::Native::prepare(&decoded)
                    }
                    .unwrap();
                    black_box(&native);
                    let elapsed = started.elapsed().as_secs_f64();
                    if reuse {
                        reused.push(elapsed)
                    } else {
                        legacy.push(elapsed)
                    }
                }
            }
            println!(
                "{}",
                serde_json::json!({"input":input,"width":decoded.width(),"height":decoded.height(),"source_bytes":bytes.len(),"scope":"preparation after shared pixel validation, not whole UI or terminal","legacy_seconds":legacy,"reused_seconds":reused})
            );
        }
    }
}
