//! Minimal direct-data Kitty graphics client. It owns only the image IDs it
//! creates; text layout and terminal transport remain with the compositor.
//! Protocol: https://sw.kovidgoyal.net/kitty/graphics-protocol/
use super::super::super::ui::Ui;
use crate::client::input::paste::{IMAGE_IDLE_TIMEOUT, PngFraming};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::DynamicImage;
use std::{
    fmt::Write as _,
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) const CACHE_BYTES: usize = 12 * 1024 * 1024;
const FALLBACK_SIDE: u32 = 1024;
// Keep the existing payload budget while retaining full-resolution images
// whenever their lossless PNG fits. Screenshots usually compress well.
const PNG_BYTES: usize = (FALLBACK_SIDE * FALLBACK_SIDE * 4) as usize + 64 * 1024;
const QUERY_ID: u32 = u32::MAX;
const IMAGE_PREFIX: u32 = 0x5353_0000;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
static NEXT_IMAGE: AtomicU32 = AtomicU32::new(0);

pub(crate) struct Native {
    pub(crate) id: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    encoded: String,
}

struct PngBuffer(Vec<u8>);
impl Write for PngBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > PNG_BYTES {
            return Err(io::Error::other("terminal PNG exceeds its bounded cache"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Native {
    /// Called after the original image's dimension/output checks and outside
    /// the display lock. Try the original pixels first; a single bounded
    /// fallback keeps noisy images from increasing the retained payload budget.
    pub(crate) fn prepare(decoded: &DynamicImage) -> Option<Arc<Self>> {
        let (width, height) = (decoded.width(), decoded.height());
        if width == 0
            || height == 0
            || width > super::MAX_DIMENSION
            || height > super::MAX_DIMENSION
        {
            return None;
        }
        if let Some(native) = Self::encode(decoded) {
            return Some(native);
        }
        // Encoding failure never retries at the same resolution, and never
        // enlarges small images. The decoded source is still owned by decode.
        if width <= FALLBACK_SIDE && height <= FALLBACK_SIDE {
            return None;
        }
        Self::encode(&decoded.thumbnail(FALLBACK_SIDE.min(width), FALLBACK_SIDE.min(height)))
    }

    fn encode(decoded: &DynamicImage) -> Option<Arc<Self>> {
        // Ordinary screenshots and photographs can be encoded directly without
        // a second full-size pixel allocation. Convert other formats once.
        let converted;
        let (pixels, color) = if let Some(pixels) = decoded.as_rgba8() {
            (pixels.as_raw(), png::ColorType::Rgba)
        } else if let Some(pixels) = decoded.as_rgb8() {
            (pixels.as_raw(), png::ColorType::Rgb)
        } else {
            converted = decoded.to_rgba8();
            (converted.as_raw(), png::ColorType::Rgba)
        };
        let (width, height) = (decoded.width(), decoded.height());
        let mut buffer = PngBuffer(Vec::new());
        // The image encoder's whole-image path buffers all compressed bytes
        // before writing. Stream scanlines into our bounded sink instead, so
        // an incompressible source stops as soon as the payload budget is hit.
        let mut encoder = png::Encoder::new(&mut buffer, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::Filter::Adaptive);
        let mut writer = encoder.write_header().ok()?;
        {
            let mut stream = writer.stream_writer().ok()?;
            stream.write_all(pixels).ok()?;
            stream.finish().ok()?;
        }
        writer.finish().ok()?;
        let id = IMAGE_PREFIX | (NEXT_IMAGE.fetch_add(1, Ordering::Relaxed) % 65535 + 1);
        Some(Arc::new(Self {
            id,
            width,
            height,
            encoded: STANDARD.encode(buffer.0),
        }))
    }

    pub(crate) fn bytes(&self) -> usize {
        self.encoded.len()
    }

    /// Fit only the available width. A tall image remains tall in the event
    /// transcript; placement crops it to the viewport as the reader scrolls.
    pub(crate) fn layout_width(&self, columns: u16, cell: CellSize) -> Option<ImageBox> {
        self.layout(columns, u16::MAX, cell)
    }

    pub(crate) fn layout(&self, columns: u16, rows: u16, cell: CellSize) -> Option<ImageBox> {
        if columns == 0 || rows == 0 || cell.width == 0 || cell.height == 0 {
            return None;
        }
        let max_width = u32::from(columns) * u32::from(cell.width);
        let max_height = u32::from(rows) * u32::from(cell.height);
        let scaled = self.width > max_width || self.height > max_height;
        let width_limited = u64::from(max_width) * u64::from(self.height)
            <= u64::from(max_height) * u64::from(self.width);
        let (width, height) = if !scaled {
            (self.width, self.height)
        } else if width_limited {
            (
                max_width,
                (u64::from(max_width) * u64::from(self.height) / u64::from(self.width)).max(1)
                    as u32,
            )
        } else {
            (
                (u64::from(max_height) * u64::from(self.width) / u64::from(self.height)).max(1)
                    as u32,
                max_height,
            )
        };
        Some(ImageBox {
            columns: width
                .div_ceil(u32::from(cell.width))
                .min(u32::from(columns)) as u16,
            rows: height.div_ceil(u32::from(cell.height)).min(u32::from(rows)) as u16,
            scaled,
            width_limited,
            cell,
        })
    }

    fn upload(&self, output: &mut String) {
        let total = self.encoded.len().div_ceil(4096);
        for (index, chunk) in self.encoded.as_bytes().chunks(4096).enumerate() {
            if index == 0 {
                let _ = write!(
                    output,
                    "\x1b_Ga=t,t=d,f=100,i={},q=1,m={};",
                    self.id,
                    u8::from(index + 1 < total)
                );
            } else {
                let _ = write!(output, "\x1b_Gq=1,m={};", u8::from(index + 1 < total));
            }
            // Base64 is ASCII, so every protocol chunk is a valid UTF-8 slice.
            output.push_str(std::str::from_utf8(chunk).unwrap());
            output.push_str("\x1b\\");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CellSize {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct ImageBox {
    pub(crate) columns: u16,
    pub(crate) rows: u16,
    scaled: bool,
    width_limited: bool,
    cell: CellSize,
}

impl ImageBox {
    /// Translate a visible row interval into a bounded source crop. The caller
    /// supplies the actual viewport intersection, never an off-screen anchor.
    pub(crate) fn placement(
        self,
        image: Arc<Native>,
        row: u16,
        col: u16,
        skip: u16,
        visible: u16,
    ) -> Option<Placement> {
        let visible = visible.min(self.rows.saturating_sub(skip));
        if visible == 0 {
            return None;
        }
        let source_row = |value: u16, round_up: bool| {
            let (numerator, denominator) = if self.scaled && self.width_limited {
                // The terminal scales by columns; the final occupied text row
                // may contain only a fraction of a cell's image pixels.
                (
                    u64::from(value) * u64::from(self.cell.height) * u64::from(image.width),
                    u64::from(self.columns) * u64::from(self.cell.width),
                )
            } else if self.scaled {
                (
                    u64::from(value) * u64::from(image.height),
                    u64::from(self.rows),
                )
            } else {
                (u64::from(value) * u64::from(self.cell.height), 1)
            };
            let source = if round_up {
                numerator.div_ceil(denominator)
            } else {
                numerator / denominator
            };
            source.min(u64::from(image.height)) as u32
        };
        let y = source_row(skip, false);
        // Rounding a height-limited crop inward would enlarge its width when
        // the terminal fits the requested rows. Round its far edge outward.
        let end = source_row(skip + visible, self.scaled && !self.width_limited);
        (end > y).then(|| Placement {
            image,
            rect: Rect {
                row,
                col,
                columns: self.columns,
                rows: visible,
                y,
                height: end - y,
                scaled: self.scaled,
                width_limited: self.width_limited,
                cell: self.cell,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    row: u16,
    col: u16,
    columns: u16,
    rows: u16,
    y: u32,
    height: u32,
    scaled: bool,
    width_limited: bool,
    cell: CellSize,
}

#[derive(Clone)]
pub(crate) struct Placement {
    image: Arc<Native>,
    rect: Rect,
}

impl Placement {
    pub(crate) fn offset_row(&mut self, top: u16) {
        self.rect.row += top;
    }

    fn same(&self, other: &Self) -> bool {
        self.image.id == other.image.id && self.rect == other.rect
    }

    fn display(&self, output: &mut String) {
        let r = self.rect;
        let _ = write!(
            output,
            "\x1b[{};{}H\x1b_Ga=p,i={},p=1,x=0,y={},w={},h={},C=1,z=1,q=1",
            r.row + 1,
            r.col + 1,
            self.image.id,
            r.y,
            self.image.width,
            r.height
        );
        if r.scaled {
            // One limiting axis avoids per-placement letterboxing/recentering
            // when only part of a scaled image is visible.
            if r.width_limited {
                let _ = write!(output, ",c={}", r.columns);
            } else {
                let _ = write!(output, ",r={}", r.rows);
            }
        }
        output.push_str("\x1b\\");
    }
}

#[derive(Default)]
pub(crate) struct Renderer {
    active: Vec<Placement>,
    retiring: Vec<u32>,
}

impl Renderer {
    /// Deletions precede the text diff; uploads/placements follow it. No image
    /// codec or large payload clone runs while the terminal State is locked.
    pub(crate) fn update(
        &mut self,
        desired: Vec<Placement>,
        cursor: (u16, u16),
        hidden: bool,
    ) -> (String, String) {
        let mut before = String::new();
        let mut after = String::new();
        for old in &self.active {
            match desired.iter().find(|next| next.image.id == old.image.id) {
                None => {
                    self.retiring.push(old.image.id);
                    let _ = write!(before, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", old.image.id);
                }
                Some(next) if !next.same(old) => {
                    let _ = write!(before, "\x1b_Ga=d,d=i,i={},p=1,q=2\x1b\\", old.image.id);
                }
                _ => {}
            }
        }
        for next in &desired {
            let old = self.active.iter().find(|old| old.image.id == next.image.id);
            if old.is_some_and(|old| next.same(old)) {
                continue;
            }
            if after.is_empty() {
                after.push_str("\x1b[?25l");
            }
            if old.is_none() {
                next.image.upload(&mut after);
            }
            next.display(&mut after);
        }
        if !after.is_empty() {
            let _ = write!(
                after,
                "\x1b[{};{}H\x1b[?25{}",
                cursor.0 + 1,
                cursor.1 + 1,
                if hidden { 'l' } else { 'h' }
            );
        }
        self.active = desired;
        (before, after)
    }

    pub(crate) fn complete(&mut self) {
        self.retiring.clear();
    }

    pub(crate) fn clear(&mut self) -> Vec<u8> {
        if self.active.is_empty() && self.retiring.is_empty() {
            return Vec::new();
        }
        // Cancel a partial APC/transfer before scoped cleanup. Never delete
        // unrelated terminal images, including those on the primary screen.
        let mut output = String::from("\x18\x1b\\");
        for image in self.active.drain(..) {
            let _ = write!(output, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", image.image.id);
        }
        for id in self.retiring.drain(..) {
            let _ = write!(output, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
        }
        output.into_bytes()
    }
}

#[derive(Default)]
pub(crate) struct Graphics {
    enabled: bool,
    started: bool,
    started_at: Option<Instant>,
    supported: bool,
    failed: bool,
    cell: Option<CellSize>,
    pending: Vec<u8>,
    pending_at: Option<Instant>,
    pasting: bool,
    paste_png: PngFraming,
    paste_last: Option<Instant>,
    discard_apc: bool,
    requests: Vec<u8>,
    changed: bool,
}

impl Graphics {
    #[cfg(test)]
    pub(crate) fn probe_for_test() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    pub(crate) fn environment(ui: &Ui) -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        let multiplexed = std::env::var_os("TMUX").is_some()
            || std::env::var_os("STY").is_some()
            || term.starts_with("screen")
            || term.starts_with("tmux");
        // TERM_PROGRAM alone is not capability evidence. Direct PTY data also
        // works over SSH when the far-end terminal actually answers this probe.
        Self {
            enabled: ui.color && !multiplexed,
            ..Self::default()
        }
    }

    pub(crate) fn prepare_native(&self) -> bool {
        // Avoid PNG encoding and cache allocation until the terminal has
        // acknowledged graphics support and reported usable cell dimensions.
        self.enabled && self.cell().is_some()
    }

    pub(crate) fn cell(&self) -> Option<CellSize> {
        (self.supported && !self.failed)
            .then_some(self.cell)
            .flatten()
    }

    pub(crate) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    pub(crate) fn take_requests(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.requests)
    }

    pub(crate) fn resized(&mut self) {
        // Once initial discovery succeeded, later resize measurements must not
        // reuse its expired startup deadline. Until the new report arrives,
        // hide native placements instead of using stale pixel dimensions.
        if self.cell().is_some() {
            self.started_at = None;
        }
        self.cell = None;
        self.changed = true;
        if self.enabled && self.started && !self.failed && self.requests.len() < 256 {
            self.requests.extend_from_slice(b"\x1b[16t");
        }
    }

    fn ready(&mut self, now: Instant) {
        if self.enabled && !self.started {
            self.started = true;
            self.started_at = Some(now);
            self.requests =
                format!("\x1b_Gi={QUERY_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[c")
                    .into_bytes();
            self.changed = true;
        }
        if !self.failed
            && self.cell().is_none()
            && self
                .started_at
                .is_some_and(|at| now.saturating_duration_since(at) > PROBE_TIMEOUT)
        {
            self.failed = true;
            self.changed = true;
        }
    }

    /// Forward real outer input progress without extending deadlines on redraw.
    pub(crate) fn paste_input_progress(&mut self, now: Instant) {
        if self.pasting && self.paste_png.is_image_prefix() {
            self.paste_last = Some(self.paste_last.map_or(now, |last| last.max(now)));
        }
    }

    /// Clear only a timed-out image paste; keep unrelated terminal reports intact.
    pub(crate) fn discard_image_paste(&mut self) -> bool {
        if !self.pasting || !self.paste_png.is_image_prefix() {
            return false;
        }
        self.pasting = false;
        self.pending.clear();
        self.pending_at = None;
        self.paste_png = PngFraming::default();
        self.paste_last = None;
        true
    }

    /// Consume only terminal reports, leaving ordinary keys and bracketed paste
    /// unchanged. This runs after raw input attachment, across arbitrary reads.
    pub(crate) fn input(&mut self, bytes: &[u8], now: Instant, flush: bool) -> Vec<u8> {
        if !self.enabled {
            return bytes.to_vec();
        }
        self.ready(now);
        if self.pasting
            && self.paste_png.is_image_prefix()
            && self
                .paste_last
                .is_some_and(|last| now.saturating_duration_since(last) >= IMAGE_IDLE_TIMEOUT)
        {
            // Match the shared input/PNG collector's recovery boundary before
            // interpreting the next batch of actual user input.
            self.discard_image_paste();
        }
        let mut output = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.pasting {
                const END: &[u8] = b"\x1b[201~";
                self.paste_last = Some(now);
                if self.pending.is_empty() && byte != 0x1b {
                    self.paste_png.feed(&[byte]);
                    output.push(byte);
                    continue;
                }
                self.pending.push(byte);
                if self.pending == END {
                    if self.paste_png.allows_end() {
                        self.pasting = false;
                        self.paste_png = PngFraming::default();
                        self.paste_last = None;
                    } else {
                        self.paste_png.feed(&self.pending);
                    }
                    output.append(&mut self.pending);
                    self.pending_at = None;
                } else if !END.starts_with(&self.pending) {
                    let restart = self.pending.last() == Some(&0x1b);
                    let end = self.pending.len() - usize::from(restart);
                    self.paste_png.feed(&self.pending[..end]);
                    output.extend_from_slice(&self.pending[..end]);
                    self.pending.drain(..end);
                }
                continue;
            }
            // Dashboard raw mode routes real Ctrl-C through Keyboard. Keep any
            // pending APC intact so a delayed report tail cannot become draft text.
            if byte == 0x03 {
                output.push(byte);
                continue;
            }
            if self.discard_apc {
                if self.pending.last() == Some(&0x1b) && byte == b'\\' {
                    self.discard_apc = false;
                    self.pending.clear();
                } else {
                    self.pending.clear();
                    if byte == 0x1b {
                        self.pending.push(byte);
                    }
                }
                continue;
            }
            if self.pending.is_empty() && byte != 0x1b {
                output.push(byte);
                continue;
            }
            if self.pending.is_empty() {
                self.pending_at = Some(now);
            }
            self.pending.push(byte);
            let apc = self.pending.starts_with(b"\x1b_");
            let csi = self.pending.starts_with(b"\x1b[");
            let complete = if apc {
                self.pending.ends_with(b"\x1b\\")
            } else if csi {
                self.pending.len() > 2 && (0x40..=0x7e).contains(&byte)
            } else {
                self.pending.len() > 1
            };
            if complete {
                let sequence = std::mem::take(&mut self.pending);
                self.pending_at = None;
                if sequence == b"\x1b[200~" {
                    self.pasting = true;
                    self.paste_png = PngFraming::default();
                    self.paste_last = Some(now);
                    output.extend_from_slice(&sequence);
                } else if sequence == b"\x1b[201~" {
                    self.pasting = false;
                    output.extend_from_slice(&sequence);
                } else if self.pasting || !self.report(&sequence, now) {
                    output.extend_from_slice(&sequence);
                }
            } else if self.pending.len() > 256 {
                if apc && !self.pasting {
                    self.discard_apc = true;
                    self.pending.clear();
                } else {
                    output.append(&mut self.pending);
                }
                self.pending_at = None;
            }
        }
        if !self.pasting
            && !self.pending.starts_with(b"\x1b_")
            && flush
            && (self.pending == b"\x1b"
                || self.pending_at.is_some_and(|at| {
                    now.saturating_duration_since(at) >= Duration::from_millis(80)
                }))
        {
            // An APC report may arrive in delayed fragments (for example over
            // SSH). Retain it until ST so its tail can never become draft text.
            // Storage is bounded above; ordinary Escape still reaches the
            // existing key parser without an extra key timeout.
            output.append(&mut self.pending);
            self.pending_at = None;
        }
        output
    }

    fn report(&mut self, sequence: &[u8], now: Instant) -> bool {
        let Ok(text) = std::str::from_utf8(sequence) else {
            return false;
        };
        if let Some(body) = text
            .strip_prefix("\x1b_G")
            .and_then(|text| text.strip_suffix("\x1b\\"))
        {
            if let Some((control, status)) = body.split_once(';') {
                let id = control
                    .split(',')
                    .find_map(|field| field.strip_prefix("i=")?.parse::<u32>().ok());
                if id == Some(QUERY_ID) {
                    if self
                        .started_at
                        .is_some_and(|at| now.saturating_duration_since(at) <= PROBE_TIMEOUT)
                        && !self.failed
                    {
                        self.supported = status == "OK";
                        self.failed = !self.supported;
                        self.changed = true;
                    }
                    return true;
                }
                if id.is_some_and(|id| id & 0xffff_0000 == IMAGE_PREFIX) {
                    if status != "OK" {
                        self.failed = true;
                        self.changed = true;
                    }
                    return true;
                }
            }
            return false;
        }
        if let Some(size) = text
            .strip_prefix("\x1b[6;")
            .and_then(|text| text.strip_suffix('t'))
        {
            let mut fields = size.split(';');
            let height = fields.next().and_then(|part| part.parse::<u16>().ok());
            let width = fields.next().and_then(|part| part.parse::<u16>().ok());
            if let (Some(height @ 1..=512), Some(width @ 1..=512), None) =
                (height, width, fields.next())
            {
                self.cell = Some(CellSize { width, height });
                self.changed = true;
            }
            return true;
        }
        if let Some(attributes) = text
            .strip_prefix("\x1b[?")
            .and_then(|text| text.strip_suffix('c'))
        {
            return attributes
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b';');
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> Graphics {
        Graphics {
            enabled: true,
            ..Graphics::default()
        }
    }

    fn sample() -> Arc<Native> {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            600,
            300,
            image::Rgba([30, 120, 210, 255]),
        ));
        Native::prepare(&image).unwrap()
    }

    #[test]
    fn resize_hides_native_pixels_until_fresh_cell_dimensions_arrive() {
        let now = Instant::now();
        let mut graphics = probe();
        let reply = format!("\x1b_Gi={QUERY_ID};OK\x1b\\\x1b[6;25;10t");
        assert!(graphics.input(reply.as_bytes(), now, false).is_empty());
        assert_eq!(
            graphics.cell(),
            Some(CellSize {
                width: 10,
                height: 25
            })
        );
        graphics.take_requests();
        graphics.take_changed();

        graphics.resized();
        assert!(graphics.take_changed());
        assert_eq!(graphics.take_requests(), b"\x1b[16t");
        assert_eq!(
            graphics.cell(),
            None,
            "old font geometry must not place an image"
        );
        assert!(!graphics.prepare_native());
        let later = now + PROBE_TIMEOUT + Duration::from_secs(1);
        assert_eq!(graphics.input(b"draft", later, true), b"draft");
        assert_eq!(
            graphics.cell(),
            None,
            "a missing measurement keeps the fallback"
        );
        assert!(graphics.input(b"\x1b[6;0;0t", later, false).is_empty());
        assert_eq!(graphics.cell(), None);
        assert!(graphics.input(b"\x1b[6;12;6t", later, false).is_empty());
        assert_eq!(
            graphics.cell(),
            Some(CellSize {
                width: 6,
                height: 12
            })
        );
        assert!(graphics.take_changed());
        assert!(graphics.prepare_native());
    }

    #[test]
    fn probe_reports_never_enter_the_draft_at_any_read_boundary() {
        let now = Instant::now();
        let response = format!("\x1b_Gi={QUERY_ID};OK\x1b\\\x1b[6;18;9t\x1b[?62;4c");
        for split in 0..=response.len() {
            let mut graphics = probe();
            assert!(graphics.input(b"", now, true).is_empty());
            assert!(!graphics.take_requests().is_empty());
            assert!(
                graphics
                    .input(&response.as_bytes()[..split], now, false)
                    .is_empty()
            );
            assert!(
                graphics
                    .input(&response.as_bytes()[split..], now, false)
                    .is_empty()
            );
            assert_eq!(
                graphics.cell(),
                Some(CellSize {
                    width: 9,
                    height: 18
                })
            );
            let keys = "中文\x1b[D\x1b[18~suffix";
            assert_eq!(graphics.input(keys.as_bytes(), now, false), keys.as_bytes());
        }
        let mut graphics = probe();
        let paste = format!("\x1b[200~{response}literal\x1b[201~");
        assert_eq!(
            graphics.input(paste.as_bytes(), now, false),
            paste.as_bytes()
        );
        assert_eq!(
            graphics.cell(),
            None,
            "pasted data is not capability evidence"
        );
        assert!(graphics.input(b"\x1b", now, false).is_empty());
        assert_eq!(graphics.input(b"", now, true), b"\x1b");
    }

    #[test]
    fn delayed_report_fragments_do_not_turn_into_user_input() {
        let now = Instant::now();
        let mut graphics = probe();
        assert!(
            graphics
                .input(b"\x1b_Gi=4294967295;", now, false)
                .is_empty()
        );
        assert!(
            graphics
                .input(b"", now + Duration::from_millis(150), true)
                .is_empty()
        );
        assert!(
            graphics
                .input(b"OK\x1b\\", now + Duration::from_millis(151), false)
                .is_empty()
        );
        assert!(graphics.input(b"\x1b[6;16;8t", now, false).is_empty());
        assert_eq!(
            graphics.cell(),
            Some(CellSize {
                width: 8,
                height: 16
            })
        );
        assert_eq!(graphics.input(b"draft", now, false), b"draft");

        let oversized = format!("\x1b_Gi={QUERY_ID};{}", "x".repeat(300));
        assert!(graphics.input(oversized.as_bytes(), now, false).is_empty());
        assert!(graphics.discard_apc && graphics.pending.len() <= 1);
        assert!(
            !graphics.discard_image_paste(),
            "image paste recovery cannot erase an unrelated pending APC"
        );
        assert!(
            graphics
                .input(b"", now + Duration::from_millis(150), true)
                .is_empty()
        );
        assert_eq!(
            graphics.input(b"\x1b\\next draft", now + Duration::from_millis(151), false),
            b"next draft"
        );
        assert!(!graphics.discard_apc);
    }

    #[test]
    fn real_interrupt_passes_through_buffered_and_discarded_apc_without_leaking_tails() {
        let now = Instant::now();
        for discard in [false, true] {
            let mut graphics = probe();
            let prefix = format!(
                "\x1b_Gi={QUERY_ID};{}",
                if discard {
                    "x".repeat(300)
                } else {
                    String::new()
                }
            );
            assert!(graphics.input(prefix.as_bytes(), now, false).is_empty());
            assert_eq!(graphics.discard_apc, discard);
            let pending = graphics.pending.clone();
            assert_eq!(
                graphics.input(b"\x03", now + Duration::from_millis(150), false),
                b"\x03",
                "real Ctrl-C must reach Keyboard even before a terminal report ends"
            );
            assert_eq!(graphics.discard_apc, discard);
            assert_eq!(graphics.pending, pending);
            assert_eq!(
                graphics.input(
                    b"OK\x1b\\next draft",
                    now + Duration::from_millis(151),
                    false
                ),
                b"next draft",
                "the APC tail stays out of the draft after passing through Ctrl-C"
            );
            assert!(!graphics.discard_apc && graphics.pending.is_empty());
        }
    }

    #[test]
    fn only_raw_image_paste_progress_extends_the_idle_deadline() {
        let now = Instant::now();
        let mut graphics = probe();
        assert_eq!(
            graphics.input(b"\x1b[200~\x89P", now, false),
            b"\x1b[200~\x89P"
        );
        let progress = now + Duration::from_secs(1);
        graphics.paste_input_progress(progress);
        graphics.paste_input_progress(now + Duration::from_millis(500));
        assert_eq!(graphics.paste_last, Some(progress), "progress is monotonic");
        assert!(
            graphics
                .input(b"", now + Duration::from_millis(2500), true)
                .is_empty()
        );
        assert!(graphics.pasting);
        assert_eq!(graphics.paste_last, Some(progress), "ticks are not input");
        assert!(
            graphics
                .input(b"", progress + IMAGE_IDLE_TIMEOUT, true)
                .is_empty()
        );
        assert!(!graphics.pasting);
    }

    #[test]
    fn binary_bracketed_paste_is_opaque_to_graphics_reports() {
        let now = Instant::now();
        let payload = crate::client::input::paste::fixture_png();
        assert!(payload.contains(&0x03), "PNG control bytes stay literal");
        image::load_from_memory_with_format(&payload, image::ImageFormat::Png).unwrap();
        let embedded_end = payload
            .windows(6)
            .position(|part| part == b"\x1b[201~")
            .unwrap()
            + 6;
        let mut input = b"\x1b[200~".to_vec();
        input.extend_from_slice(&payload);
        input.extend_from_slice(b"\x1b[201~suffix");
        for size in 1..=input.len() {
            let mut graphics = probe();
            let mut output = Vec::new();
            for part in input.chunks(size) {
                output.extend(graphics.input(part, now, false));
            }
            assert_eq!(output, input, "binary paste chunk size {size}");
            assert!(!graphics.pasting);
            assert_eq!(graphics.cell(), None);
        }
        let mut graphics = probe();
        let _ = graphics.input(b"\x1b[200~", now, false);
        assert_eq!(
            graphics.input(&payload[..embedded_end], now, false),
            payload[..embedded_end]
        );
        assert!(graphics.pasting, "a delimiter inside a PNG chunk is data");
        assert!(!graphics.paste_png.allows_end());
        assert_eq!(
            graphics.input(b"next draft", now + IMAGE_IDLE_TIMEOUT, false),
            b"next draft"
        );
        assert!(
            !graphics.pasting,
            "incomplete PNG timeout recovers before new input"
        );
        for bytes in [b"\x89P".as_slice(), payload.as_slice()] {
            let mut graphics = probe();
            assert_eq!(graphics.input(b"\x1b[200~", now, false), b"\x1b[200~");
            assert_eq!(graphics.input(bytes, now, false), bytes);
            assert!(graphics.pasting && graphics.paste_png.is_image_prefix());
            assert!(
                graphics
                    .input(b"", now + IMAGE_IDLE_TIMEOUT, true)
                    .is_empty()
            );
            assert!(
                !graphics.pasting,
                "partial signature and complete PNG without outer End share the same idle recovery"
            );
            assert_eq!(
                graphics.input(b"next draft", now + IMAGE_IDLE_TIMEOUT, false),
                b"next draft"
            );
        }
    }

    #[test]
    fn timeout_and_native_errors_keep_the_fallback_available() {
        let now = Instant::now();
        let mut graphics = probe();
        graphics.input(b"", now, true);
        assert!(
            !graphics.prepare_native(),
            "unanswered probes must not allocate native images"
        );
        let response = format!("\x1b_Gi={QUERY_ID};OK\x1b\\\x1b[6;16;8t");
        let expired = now + PROBE_TIMEOUT + Duration::from_millis(1);
        assert!(graphics.input(b"", expired, true).is_empty());
        assert!(
            graphics.failed,
            "a silent terminal must stop native preparation after timeout"
        );
        assert!(!graphics.prepare_native());
        assert!(
            graphics
                .input(response.as_bytes(), expired, false)
                .is_empty()
        );
        assert_eq!(
            graphics.cell(),
            None,
            "late reports cannot revive an expired probe"
        );
        assert!(!graphics.prepare_native());
        let paste = b"\x1b[200~\x89PNG\r\n\x1a\n\x00\x03\xff\x1b[18~\x1b[201~";
        assert_eq!(graphics.input(paste, expired, false), paste);
        assert_eq!(
            graphics.input(b"user\x1b[Dinput", expired, false),
            b"user\x1b[Dinput"
        );
        let mut graphics = probe();
        graphics.input(response.as_bytes(), now, false);
        assert!(graphics.cell().is_some());
        assert!(
            graphics.prepare_native(),
            "only acknowledged support enables native encoding"
        );
        assert!(
            graphics
                .input(
                    b"\x1b_Gi=1397948417;ENOSPC:no image space\x1b\\",
                    now,
                    false
                )
                .is_empty()
        );
        assert_eq!(graphics.cell(), None);
        assert!(!graphics.prepare_native());
    }

    #[test]
    fn placements_crop_inside_the_viewport_and_release_only_owned_images() {
        let image = sample();
        let cell = CellSize {
            width: 10,
            height: 20,
        };
        let bounds = image.layout(20, 5, cell).unwrap();
        assert_eq!((bounds.columns, bounds.rows), (20, 5));
        let full = bounds.placement(image.clone(), 3, 4, 0, 5).unwrap();
        let crop = bounds.placement(image.clone(), 3, 4, 2, 3).unwrap();
        assert_eq!((crop.rect.y, crop.rect.height), (120, 180));
        assert!(crop.rect.row + crop.rect.rows <= 8);
        let mut renderer = Renderer::default();
        let (before, after) = renderer.update(vec![full.clone()], (20, 2), false);
        assert!(before.is_empty());
        assert!(after.contains("a=t,t=d,f=100"));
        assert!(after.contains("C=1,z=1,q=1,c=20\x1b\\"));
        assert!(after.ends_with("\x1b[21;3H\x1b[?25h"));
        assert_eq!(
            renderer.update(vec![full], (20, 2), false),
            (String::new(), String::new())
        );
        let (before, after) = renderer.update(vec![crop], (20, 2), false);
        assert!(before.contains(&format!("d=i,i={},p=1", image.id)));
        assert!(!after.contains("a=t"), "scrolling reuses uploaded pixels");
        assert!(after.contains("y=120,w=600,h=180"));
        let clear = String::from_utf8(renderer.clear()).unwrap();
        assert!(clear.contains(&format!("d=I,i={}", image.id)));
        assert!(!clear.contains("d=A"));
        assert!(renderer.clear().is_empty());
    }

    #[test]
    fn scaled_partial_rows_use_pixel_geometry_without_recentering_or_overflow() {
        let image =
            Native::prepare(&DynamicImage::ImageRgba8(image::RgbaImage::new(128, 64))).unwrap();
        let cell = CellSize {
            width: 8,
            height: 16,
        };
        let bounds = image.layout(10, 8, cell).unwrap();
        assert_eq!((bounds.columns, bounds.rows), (10, 3)); // 80 x 40 pixels
        let crop = bounds.placement(image.clone(), 4, 2, 1, 2).unwrap();
        assert_eq!((crop.rect.y, crop.rect.height), (25, 39));
        let tail = bounds.placement(image, 4, 2, 2, 1).unwrap();
        assert_eq!((tail.rect.y, tail.rect.height), (51, 13));
        let mut wire = String::new();
        tail.display(&mut wire);
        assert!(wire.contains(",c=10"));
        assert!(
            !wire.contains(",r="),
            "cell rounding must not vertically center a crop"
        );
        assert!(tail.rect.height * 80 / 128 <= u32::from(cell.height));

        // An inward-rounded source crop would expand 1024 source pixels beyond
        // the 1020-pixel viewport when only one height-limited row is visible.
        let image =
            Native::prepare(&DynamicImage::ImageRgba8(image::RgbaImage::new(1024, 200))).unwrap();
        let cell = CellSize {
            width: 1,
            height: 1,
        };
        let bounds = image.layout(1020, 199, cell).unwrap();
        let crop = bounds.placement(image, 2, 0, 1, 1).unwrap();
        assert_eq!((crop.rect.y, crop.rect.height), (1, 2));
        assert!(crop.image.width * u32::from(crop.rect.rows) / crop.rect.height <= 1020);
        let mut wire = String::new();
        crop.display(&mut wire);
        assert!(wire.contains(",r=1"));
        assert!(!wire.contains(",c="));
    }

    #[test]
    fn native_png_is_bounded_and_protocol_chunks_decode_to_the_same_pixels() {
        let source = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(2048, 1024, |x, y| {
            image::Rgba([if x % 2 == 0 { 10 } else { 240 }, (y % 256) as u8, 90, 200])
        }));
        let native = Native::prepare(&source).unwrap();
        assert_eq!((native.width, native.height), (2048, 1024));
        assert!(native.bytes() <= PNG_BYTES.div_ceil(3) * 4);
        let mut wire = String::new();
        native.upload(&mut wire);
        let chunks = wire.split("\x1b_G").skip(1).collect::<Vec<_>>();
        let mut encoded = String::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let (header, payload) = chunk
                .strip_suffix("\x1b\\")
                .unwrap()
                .split_once(';')
                .unwrap();
            assert!(payload.len() <= 4096);
            assert_eq!(payload.len() % 4, 0);
            assert!(header.contains(if index + 1 < chunks.len() {
                "m=1"
            } else {
                "m=0"
            }));
            encoded.push_str(payload);
        }
        let decoded = image::load_from_memory(&STANDARD.decode(encoded).unwrap())
            .unwrap()
            .into_rgba8();
        assert_eq!(decoded.dimensions(), (2048, 1024));
        assert!(
            decoded.as_raw() == source.as_rgba8().unwrap().as_raw(),
            "one-pixel details must survive native preparation and protocol transmission"
        );
        let tiny = Native::prepare(&DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))).unwrap();
        let area = tiny
            .layout(
                80,
                20,
                CellSize {
                    width: 8,
                    height: 16,
                },
            )
            .unwrap();
        assert!(!area.scaled);
        let placement = area.placement(tiny, 3, 4, 0, 1).unwrap();
        let mut wire = String::new();
        placement.display(&mut wire);
        assert!(
            !wire.contains(",c="),
            "tiny native images keep their actual pixel size"
        );
    }

    #[test]
    fn noisy_native_images_fall_back_without_increasing_the_payload_budget() {
        let mut state = 0x3141_5926u32;
        let source = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(1200, 1000, |_, _| {
            image::Rgba(std::array::from_fn(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            }))
        }));
        assert!(Native::encode(&source).is_none());
        let native = Native::prepare(&source).unwrap();
        assert!(native.width <= FALLBACK_SIDE && native.height <= FALLBACK_SIDE);
        assert!(native.width < source.width() && native.height < source.height());
        let png = STANDARD.decode(&native.encoded).unwrap();
        assert!(png.len() <= PNG_BYTES);
        assert!(native.bytes() <= PNG_BYTES.div_ceil(3) * 4);
        assert_eq!(
            image::load_from_memory(&png)
                .unwrap()
                .into_rgba8()
                .dimensions(),
            (native.width, native.height)
        );
    }

    #[test]
    fn width_fitted_tall_images_scroll_at_a_constant_scale() {
        let source = DynamicImage::ImageRgba8(image::RgbaImage::new(1200, 2400));
        let image = Native::prepare(&source).unwrap();
        let bounds = image
            .layout_width(
                60,
                CellSize {
                    width: 10,
                    height: 20,
                },
            )
            .unwrap();
        assert_eq!((bounds.columns, bounds.rows), (60, 60));
        let first = bounds.placement(image.clone(), 4, 2, 0, 20).unwrap();
        let next = bounds.placement(image, 4, 2, 10, 20).unwrap();
        assert_eq!((first.rect.y, first.rect.height), (0, 800));
        assert_eq!((next.rect.y, next.rect.height), (400, 800));
        let mut wire = String::new();
        next.display(&mut wire);
        assert!(wire.contains(",c=60"));
        assert!(!wire.contains(",r="));
    }
}
