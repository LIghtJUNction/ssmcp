//! A cached square raster avoids font-dependent gaps in QR modules.
use super::super::images::Native;
use image::{DynamicImage, Rgba, RgbaImage};
use qrcodegen::QrCode;
use std::sync::Arc;

pub(super) const MODULE_PIXELS: u32 = 8;

pub(super) fn prepare(code: &QrCode) -> Option<Arc<Native>> {
    Native::prepare(&DynamicImage::ImageRgba8(raster(code)))
}

fn raster(code: &QrCode) -> RgbaImage {
    let side = (code.size() + 2 * super::QUIET) as u32 * MODULE_PIXELS;
    RgbaImage::from_fn(side, side, |x, y| {
        let dark = code.get_module(
            (x / MODULE_PIXELS) as i32 - super::QUIET,
            (y / MODULE_PIXELS) as i32 - super::QUIET,
        );
        let value = if dark { 0 } else { 255 };
        Rgba([value, value, value, 255])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use qrcodegen::QrCodeEcc;

    #[test]
    fn every_module_is_square_with_an_opaque_four_module_quiet_zone() {
        let code = QrCode::encode_text("https://ssmcp.lmm.best/guide/", QrCodeEcc::Medium).unwrap();
        let pixels = raster(&code);
        let side = (code.size() + 8) as u32 * MODULE_PIXELS;
        assert_eq!(pixels.dimensions(), (side, side));
        let quiet = 4 * MODULE_PIXELS;
        for (x, y, pixel) in pixels.enumerate_pixels() {
            assert_eq!(pixel.0[3], 255);
            if x < quiet || y < quiet || x >= side - quiet || y >= side - quiet {
                assert_eq!(pixel.0, [255; 4], "quiet zone must be fully white");
            }
        }
        // The top-left finder has a black outer ring and a 3x3 black center.
        // Check whole pixels, including module/cell boundaries where font
        // rendering previously left gaps.
        for y in 0..7 * MODULE_PIXELS {
            for x in 0..7 * MODULE_PIXELS {
                let (mx, my) = (x / MODULE_PIXELS, y / MODULE_PIXELS);
                let dark = mx == 0
                    || mx == 6
                    || my == 0
                    || my == 6
                    || ((2..=4).contains(&mx) && (2..=4).contains(&my));
                let value = if dark { 0 } else { 255 };
                assert_eq!(
                    pixels.get_pixel(quiet + x, quiet + y).0,
                    [value, value, value, 255]
                );
            }
        }
        let native = prepare(&code).unwrap();
        assert_eq!((native.width, native.height), (side, side));
        assert!(native.bytes() < 16 * 1024);
    }
}
