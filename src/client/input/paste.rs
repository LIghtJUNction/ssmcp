//! Bounded PNG container framing for raw bracketed paste.
//! This recognizes chunk lengths and IEND, not image validity; the image importer
//! still validates the codec. A literal terminal end marker inside a chunk is data.
use std::time::Duration;

pub(crate) const MAX_IMAGE_PASTE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const IMAGE_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Debug, Default)]
pub(crate) struct PngFraming {
    prefix: usize,
    other: bool,
    header: [u8; 8],
    used: usize,
    remaining: usize,
    chunks: usize,
    bytes: usize,
    final_chunk: bool,
    complete: bool,
    invalid: bool,
}

impl PngFraming {
    pub(crate) fn is_png(&self) -> bool {
        !self.other && self.prefix == SIGNATURE.len()
    }

    pub(crate) fn allows_end(&self) -> bool {
        !self.is_png() || (self.complete && !self.invalid)
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.other || self.invalid {
                return;
            }
            self.bytes = self.bytes.saturating_add(1);
            if self.bytes > MAX_IMAGE_PASTE_BYTES {
                self.invalid = true;
                return;
            }
            if self.prefix < SIGNATURE.len() {
                if byte != SIGNATURE[self.prefix] {
                    self.other = true;
                    return;
                }
                self.prefix += 1;
                continue;
            }
            if self.complete {
                // Do not release trailing binary as keyboard input.
                self.complete = false;
                self.invalid = true;
                return;
            }
            if self.remaining > 0 {
                self.remaining -= 1;
                if self.remaining == 0 && self.final_chunk {
                    self.complete = true;
                }
                continue;
            }
            self.header[self.used] = byte;
            self.used += 1;
            if self.used < self.header.len() {
                continue;
            }
            self.used = 0;
            let length = u32::from_be_bytes(self.header[..4].try_into().unwrap()) as usize;
            let kind = &self.header[4..];
            self.final_chunk = kind == b"IEND";
            if length > MAX_IMAGE_PASTE_BYTES.saturating_sub(self.bytes + 4)
                || !kind.iter().all(u8::is_ascii_alphabetic)
                || (self.chunks == 0 && (kind != b"IHDR" || length != 13))
                || (self.chunks > 0 && kind == b"IHDR")
                || (self.final_chunk && length != 0)
            {
                self.invalid = true;
                return;
            }
            self.chunks += 1;
            self.remaining = length + 4; // payload plus CRC; no payload is retained here.
        }
    }
}

#[cfg(test)]
pub(crate) fn fixture_png() -> Vec<u8> {
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    let mut cursor = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([20, 80, 160, 128])))
        .write_to(&mut cursor, ImageFormat::Png)
        .unwrap();
    let mut png = cursor.into_inner();
    // A valid private ancillary chunk with bytes that must never become keys.
    let payload =
        b"\0\x03\x11\x13\x1a\xff\x80\r\n\x1b[200~\x1b[201~\x1b[18~\x1b[15~\x1b_Gi=1;OK\x1b\\";
    let mut chunk = (payload.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(b"ssMc");
    chunk.extend_from_slice(payload);
    let mut crc = !0u32;
    for &byte in &chunk[4..] {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    drop(png.splice(33..33, chunk));
    png
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_framing_is_bounded_and_independent_of_fragment_sizes() {
        let png = fixture_png();
        image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        let embedded = png
            .windows(6)
            .position(|part| part == b"\x1b[201~")
            .unwrap();
        for size in [1, 2, 7, 4096] {
            let mut framing = PngFraming::default();
            for part in png[..embedded].chunks(size) {
                framing.feed(part);
            }
            assert!(framing.is_png());
            assert!(!framing.allows_end());
            for part in png[embedded..].chunks(size) {
                framing.feed(part);
            }
            assert!(framing.allows_end());
            framing.feed(b"\0");
            assert!(!framing.allows_end());
        }
    }

    #[test]
    fn png_framing_rejects_truncated_or_impossible_chunks_without_allocating() {
        let png = fixture_png();
        let mut truncated = PngFraming::default();
        truncated.feed(&png[..png.len() - 1]);
        assert!(truncated.is_png());
        assert!(!truncated.allows_end());
        let mut invalid = PngFraming::default();
        invalid.feed(SIGNATURE);
        invalid.feed(&[255, 255, 255, 255, b'I', b'H', b'D', b'R']);
        assert!(!invalid.allows_end());
        invalid.feed(b"\x1b[201~\r");
        assert!(!invalid.allows_end());
        let mut text = PngFraming::default();
        text.feed("普通文字".as_bytes());
        assert!(!text.is_png());
        assert!(text.allows_end());
    }
}
