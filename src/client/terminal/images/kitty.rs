//! Minimal direct-data Kitty graphics client. It owns only the image IDs it
//! creates; text layout and terminal transport remain with the compositor.
//! Protocol: https://sw.kovidgoyal.net/kitty/graphics-protocol/
use super::super::super::ui::Ui;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageEncoder};
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
const NATIVE_SIDE: u32 = 1024;
const PNG_BYTES: usize = (NATIVE_SIDE * NATIVE_SIDE * 4) as usize + 64 * 1024;
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
    /// the display lock. Retain one static RGBA8 frame; never enlarge it.
    pub(crate) fn prepare(decoded: &DynamicImage) -> Option<Arc<Self>> {
        let pixels = decoded
            .thumbnail(
                NATIVE_SIDE.min(decoded.width()),
                NATIVE_SIDE.min(decoded.height()),
            )
            .into_rgba8();
        let (width, height) = pixels.dimensions();
        let mut png = PngBuffer(Vec::new());
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                pixels.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .ok()?;
        let id = IMAGE_PREFIX | (NEXT_IMAGE.fetch_add(1, Ordering::Relaxed) % 65535 + 1);
        Some(Arc::new(Self {
            id,
            width,
            height,
            encoded: STANDARD.encode(png.0),
        }))
    }

    pub(crate) fn bytes(&self) -> usize {
        self.encoded.len()
    }

    pub(crate) fn layout(&self, columns: u16, rows: u16, cell: CellSize) -> Option<ImageBox> {
        if columns == 0 || rows == 0 {
            return None;
        }
        let max_width = u32::from(columns) * u32::from(cell.width);
        let max_height = u32::from(rows) * u32::from(cell.height);
        let scaled = self.width > max_width || self.height > max_height;
        let (width, height) = if !scaled {
            (self.width, self.height)
        } else if u64::from(max_width) * u64::from(self.height)
            <= u64::from(max_height) * u64::from(self.width)
        {
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
        let source_row = |value: u16| {
            if self.scaled {
                u32::from(value) * image.height / u32::from(self.rows)
            } else {
                u32::from(value) * u32::from(self.cell.height)
            }
            .min(image.height)
        };
        let y = source_row(skip);
        let end = source_row(skip + visible);
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
            let _ = write!(output, ",c={},r={}", r.columns, r.rows);
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
    discard_apc: bool,
    requests: Vec<u8>,
    changed: bool,
}

impl Graphics {
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

    /// Consume only terminal reports, leaving ordinary keys and bracketed paste
    /// unchanged. This runs after raw input attachment, across arbitrary reads.
    pub(crate) fn input(&mut self, bytes: &[u8], now: Instant, flush: bool) -> Vec<u8> {
        if !self.enabled {
            return bytes.to_vec();
        }
        self.ready(now);
        let mut output = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.pasting {
                const END: &[u8] = b"\x1b[201~";
                if self.pending.is_empty() && byte != 0x1b {
                    output.push(byte);
                    continue;
                }
                self.pending.push(byte);
                if self.pending == END {
                    output.append(&mut self.pending);
                    self.pasting = false;
                    self.pending_at = None;
                } else if !END.starts_with(&self.pending) {
                    let restart = self.pending.last() == Some(&0x1b);
                    let end = self.pending.len() - usize::from(restart);
                    output.extend_from_slice(&self.pending[..end]);
                    self.pending.drain(..end);
                }
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
            && flush
            && (self.pending == b"\x1b"
                || self.pending_at.is_some_and(|at| {
                    now.saturating_duration_since(at) >= Duration::from_millis(80)
                }))
        {
            // Only an identified APC report may be dropped after an interrupted
            // response. A lone Escape still reaches the existing key parser.
            if !self.pending.starts_with(b"\x1b_G") || self.pasting {
                output.append(&mut self.pending);
            } else {
                self.pending.clear();
            }
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
        assert!(after.contains("C=1,z=1,q=1,c=20,r=5"));
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
    fn native_png_is_bounded_and_protocol_chunks_decode_to_the_same_pixels() {
        let source = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2048,
            1024,
            image::Rgba([10, 40, 90, 200]),
        ));
        let native = Native::prepare(&source).unwrap();
        assert_eq!((native.width, native.height), (1024, 512));
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
        assert_eq!(decoded.dimensions(), (1024, 512));
        assert_eq!(decoded.get_pixel(0, 0).0, [10, 40, 90, 200]);
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
}
