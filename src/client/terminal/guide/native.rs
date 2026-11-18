//! A cached square raster avoids font-dependent gaps in QR modules.
use super::super::images::{CellSize, Native};
use image::{DynamicImage, Rgba, RgbaImage};
use qrcodegen::QrCode;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

pub(super) const MODULE_PIXELS: u32 = 8;

/// Each QR alternative is encoded at most once, and only after it can be shown.
pub(super) struct Cache {
    entries: Vec<Arc<Entry>>,
}

struct Entry {
    code: QrCode,
    image: OnceLock<Option<Arc<Native>>>,
    started: AtomicBool,
}

impl Cache {
    pub(super) fn new(codes: Vec<QrCode>) -> Self {
        Self {
            entries: codes
                .into_iter()
                .map(|code| {
                    Arc::new(Entry {
                        code,
                        image: OnceLock::new(),
                        started: AtomicBool::new(false),
                    })
                })
                .collect(),
        }
    }

    pub(super) fn image(&self, columns: u16, rows: u16, cell: CellSize) -> Option<&Arc<Native>> {
        let entry = self.entries.iter().find(|entry| {
            let side = (entry.code.size() + 2 * super::QUIET) as u32 * MODULE_PIXELS;
            side <= u32::from(columns) * u32::from(cell.width)
                && side <= u32::from(rows) * u32::from(cell.height)
        })?;
        if entry.image.get().is_none() && !entry.started.swap(true, Ordering::AcqRel) {
            let work = Arc::clone(entry);
            // No terminal state is held by this short, one-shot worker.
            if std::thread::Builder::new()
                .name("ssmcp-guide-qr".into())
                .spawn(move || {
                    let _ = work.image.set(prepare(&work.code));
                    super::super::invalidate();
                })
                .is_err()
            {
                let _ = entry.image.set(None);
            }
        }
        entry.image.get()?.as_ref()
    }

    #[cfg(test)]
    pub(super) fn prepare_for_test(&self) {
        for entry in &self.entries {
            let _ = entry.image.set(prepare(&entry.code));
        }
    }
}

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
    fn unsupported_or_cramped_guide_does_not_request_native_encoding() {
        let guide = super::super::Guide::from_endpoint("https://ssmcp.lmm.best/mcp").unwrap();
        let ui = super::super::Ui::default();
        let cell = CellSize {
            width: 8,
            height: 16,
        };
        for (width, height, measured) in
            [(37, 29, None), (20, 29, Some(cell)), (37, 12, Some(cell))]
        {
            let (rows, placement) = guide.rows(width, height, &ui, measured, (0, 0));
            assert!(placement.is_none());
            assert!(!rows.is_empty());
            assert!(
                guide
                    .codes
                    .entries
                    .iter()
                    .all(|entry| !entry.started.load(Ordering::Acquire)
                        && entry.image.get().is_none())
            );
        }
    }

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
