//! Local clipboard/import jobs produce ordinary @references, never a second wire queue.
use super::references::files;
use crate::{output::MAX_IMAGE_BYTES, workspace::Workspace};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageFormat, ImageReader};
use serde_json::{Value, json};
use std::{
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};
use tokio::{sync::oneshot, task::JoinHandle};

pub(super) const MAX_SOURCE_BYTES: usize = super::input::paste::MAX_IMAGE_PASTE_BYTES;
const MAX_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_DIMENSION: u32 = 16_384;
pub(super) const REFERENCE_RESERVE: usize = 64;

pub(crate) enum Source {
    Clipboard,
    Path(String),
    Files(Vec<PathBuf>),
    Text(String),
    Image(Vec<u8>),
    Copy(String),
    Handoff(String),
    Context { command: String, arguments: String },
}
#[derive(Debug)]
pub(crate) enum Loaded {
    Text(String),
    Reference {
        reference: String,
        description: String,
    },
    Files {
        text: String,
        description: String,
        files: files::ImportedFiles,
    },
    Context(String),
    Copied,
}
// An attach frontend owns local input files without owning the service's
// Runtime, tools or model connection. Only an explicit, scoped binding enables
// this fallback; ordinary live clients keep their existing Runtime path.
static LOCAL_INPUT: Mutex<Option<Weak<Workspace>>> = Mutex::new(None);

pub(crate) struct LocalInput {
    workspace: Arc<Workspace>,
}

impl LocalInput {
    pub(crate) fn bind(root: PathBuf) -> Result<Self, String> {
        let workspace = Arc::new(Workspace::new(root)?);
        let mut binding = LOCAL_INPUT
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if binding.as_ref().and_then(Weak::upgrade).is_some() {
            return Err("Local input is already bound to an attached workspace".into());
        }
        *binding = Some(Arc::downgrade(&workspace));
        Ok(Self { workspace })
    }
}

impl Drop for LocalInput {
    fn drop(&mut self) {
        let mut binding = LOCAL_INPUT
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if binding
            .as_ref()
            .is_some_and(|current| current.ptr_eq(&Arc::downgrade(&self.workspace)))
        {
            *binding = None;
        }
    }
}

pub(super) struct Task {
    receiver: oneshot::Receiver<Result<Loaded, String>>,
    handle: JoinHandle<()>,
}
impl Task {
    pub(super) fn start(source: Source) -> Result<Self, String> {
        let (sender, receiver) = oneshot::channel();
        let handle = match super::interface::harness::local_runtime() {
            Ok((runtime, handle)) => handle.spawn(async move {
                let result = runtime.import_input(source).await;
                let _ = sender.send(result);
                super::terminal::invalidate();
            }),
            Err(error) => {
                // Context and handoff depend on the connected client's state;
                // the attach scope grants only explicit local input operations.
                if !matches!(
                    &source,
                    Source::Clipboard
                        | Source::Path(_)
                        | Source::Files(_)
                        | Source::Text(_)
                        | Source::Image(_)
                        | Source::Copy(_)
                ) {
                    return Err(error);
                }
                let workspace = LOCAL_INPUT
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(error)?;
                let handle = tokio::runtime::Handle::try_current()
                    .map_err(|_| "Local input requires the interactive attach runtime")?;
                handle.spawn(async move {
                    let result = load(source, &workspace).await;
                    let _ = sender.send(result);
                    super::terminal::invalidate();
                })
            }
        };
        Ok(Self { receiver, handle })
    }
    #[cfg(test)]
    pub(super) fn pending_for_test() -> (Self, oneshot::Sender<Result<Loaded, String>>) {
        let (sender, receiver) = oneshot::channel();
        let handle = tokio::spawn(std::future::pending::<()>());
        (Self { receiver, handle }, sender)
    }

    pub(super) fn poll(&mut self) -> Option<Result<Loaded, String>> {
        match self.receiver.try_recv() {
            Ok(value) => Some(value),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err(
                "Local input loading stopped; the draft is preserved".into(),
            )),
        }
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Explicit absolute file selections are copied; workspace-relative paths retain
/// the existing rooted reference behavior. Plain pasted prose is never inferred.
pub(super) fn source_argument(text: &str) -> Result<Source, String> {
    if let Some(paths) = files::pasted_paths(text) {
        return Ok(Source::Files(paths));
    }
    path_argument(text).map(Source::Path)
}

/// Apply the image budget only after its signature is available. Text keeps
/// the existing 4 MiB limit; both paths stay bounded while reads are fragmented.
pub(super) fn paste_source(bytes: Vec<u8>) -> Result<Source, String> {
    if crate::output::image_mime_type(&bytes).is_some() {
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err("Image paste exceeds 16 MiB; use a smaller image or /attach PATH".into());
        }
        return Ok(Source::Image(bytes));
    }
    let head = &bytes[..bytes.len().min(96)];
    let damaged_png = head.starts_with(b"\xef\xbf\xbdPNG")
        || head.starts_with(b"\xc2\x89PNG")
        || (head.starts_with(b"PNG") && head.windows(4).any(|part| part == b"IHDR"));
    let damaged_jpeg = head.starts_with(b"\xef\xbf\xbd\xef\xbf\xbd")
        && head
            .windows(4)
            .any(|part| part == b"JFIF" || part == b"Exif");
    let damaged_webp = head.starts_with(b"RIFF") && head.windows(4).any(|part| part == b"WEBP");
    if damaged_png || damaged_jpeg || damaged_webp {
        return Err("The terminal altered image bytes; paste was not saved as text. Copy the original image and use Ctrl+V or /paste, or use /attach PATH.".into());
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| "Paste contains binary data that is not a supported image. Use Ctrl+V or /paste for an image, or /attach PATH for a file.")?;
    if text.contains('\0') {
        return Err(
            "Paste contains binary data, not plain text. Use Ctrl+V or /paste, or /attach PATH."
                .into(),
        );
    }
    if text.len() > super::references::paste::MAX_PASTE_BYTES {
        return Err("Paste exceeds 4 MiB of UTF-8 text; save it as a file and use @path".into());
    }
    Ok(Source::Text(text))
}

/// Inspect only a fixed header while collecting; images and text keep separate caps.
pub(super) fn paste_limit(current: &[u8], incoming: &[u8]) -> usize {
    let mut head = [0u8; 16];
    let mut count = 0;
    for byte in current.iter().chain(incoming).take(head.len()) {
        head[count] = *byte;
        count += 1;
    }
    if crate::output::image_mime_type(&head[..count]).is_some() {
        MAX_SOURCE_BYTES
    } else {
        super::references::paste::MAX_PASTE_BYTES
    }
}

/// Parse one explicit path, with quotes/escaped spaces, without shell expansion.
pub(super) fn path_argument(text: &str) -> Result<String, String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in text.trim().chars() {
        if escaped {
            part.push(ch);
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') && cfg!(not(windows)) {
            escaped = true;
        } else if Some(ch) == quote {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if quote.is_none() && ch.is_whitespace() {
            if !part.is_empty() {
                parts.push(std::mem::take(&mut part));
            }
        } else {
            part.push(ch);
        }
    }
    if escaped || quote.is_some() {
        return Err("Close the quoted path before using /attach".into());
    }
    if !part.is_empty() {
        parts.push(part);
    }
    if parts.len() != 1 {
        return Err(
            "Use /attach PATH for one workspace file; quote paths containing spaces".into(),
        );
    }
    let mut path = parts.pop().unwrap();
    if path.starts_with("file://") {
        path = reqwest::Url::parse(&path)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .and_then(|path| path.into_os_string().into_string().ok())
            .ok_or("Use a local UTF-8 file path")?;
    }
    if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
        return Err(
            "Use a nonempty file path of at most 4096 bytes without control characters".into(),
        );
    }
    Ok(path)
}
fn quoted(path: &str) -> String {
    format!("@\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(crate) async fn load(source: Source, workspace: &Workspace) -> Result<Loaded, String> {
    match source {
        Source::Copy(text) => super::clipboard::write_text(&text)
            .await
            .map(|()| Loaded::Copied),
        Source::Clipboard => match super::clipboard::read().await? {
            super::clipboard::Content::Text(text) if text.len() > 8 * 1024 => {
                store_text(workspace.root().to_path_buf(), text).await
            }
            #[cfg(all(not(target_os = "macos"), not(windows)))]
            super::clipboard::Content::Files(paths) => {
                load_files(workspace.root().to_path_buf(), paths).await
            }
            super::clipboard::Content::Text(text) => Ok(Loaded::Text(text)),
            super::clipboard::Content::Image {
                name,
                mime_type,
                bytes,
            } => store_image(workspace.root().to_path_buf(), name, mime_type, bytes).await,
        },
        Source::Path(path) => load_path(workspace, &path).await,
        Source::Files(paths) => load_files(workspace.root().to_path_buf(), paths).await,
        Source::Text(text) => store_text(workspace.root().to_path_buf(), text).await,
        Source::Image(bytes) => {
            store_image(
                workspace.root().to_path_buf(),
                "pasted-image".into(),
                String::new(),
                bytes,
            )
            .await
        }
        Source::Context { command, arguments } => tokio::task::spawn_blocking(move || {
            super::context_memory::command(&command, &arguments).map(Loaded::Context)
        })
        .await
        .map_err(|error| format!("Context operation failed: {error}"))?,
        Source::Handoff(note) => {
            let root = workspace.root().to_path_buf();
            tokio::task::spawn_blocking(move || {
                super::handoff::prepare(&root, &note).map(|reference| Loaded::Reference {
                    reference,
                    description: "Handoff prepared · review the reference before sending".into(),
                })
            })
            .await
            .map_err(|error| format!("Handoff preparation failed: {error}"))?
        }
    }
}

async fn store_text(root: PathBuf, text: String) -> Result<Loaded, String> {
    tokio::task::spawn_blocking(move || {
        super::references::paste::store_at(&root, &text).map(|reference| Loaded::Reference {
            reference,
            description: format!("Saved long text · {} UTF-8 bytes", text.len()),
        })
    })
    .await
    .map_err(|error| format!("Text storage failed: {error}"))?
}

async fn load_files(root: PathBuf, paths: Vec<PathBuf>) -> Result<Loaded, String> {
    tokio::task::spawn_blocking(move || load_files_at(&root, &paths))
        .await
        .map_err(|error| format!("File import failed: {error}"))?
}

fn load_files_at(root: &Path, paths: &[PathBuf]) -> Result<Loaded, String> {
    let selected = files::read(paths)?;
    let mut prepared = Vec::with_capacity(selected.len());
    let mut labels = Vec::new();
    for mut file in selected {
        if let Some(mime) = crate::output::image_mime_type(&file.bytes) {
            let image = prepare(&file.name, mime, file.bytes, MAX_IMAGE_BYTES)?;
            labels.push(image.label);
            if image.mime_type != mime {
                let extension = match image.mime_type {
                    "image/png" => "png",
                    "image/jpeg" => "jpg",
                    "image/gif" => "gif",
                    "image/webp" => "webp",
                    _ => return Err("Unsupported processed image format".into()),
                };
                let mut name = PathBuf::from(&file.name);
                name.set_extension(extension);
                file.name = name.to_string_lossy().into_owned();
            }
            file.bytes = image.bytes;
        }
        prepared.push(file);
    }
    // Validate and decode every source before creating any owned destination.
    let imported = files::import(root, prepared)?;
    let count = imported.entries.len();
    let bytes: u64 = imported.entries.iter().map(|entry| entry.bytes).sum();
    let names = imported
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let description = format!(
        "Imported {count} files · {bytes} bytes · {names}{}",
        if labels.is_empty() {
            String::new()
        } else {
            format!(" · {}", labels.join("; "))
        }
    );
    Ok(Loaded::Files {
        text: imported.references(),
        description,
        files: imported,
    })
}

async fn load_path(workspace: &Workspace, supplied: &str) -> Result<Loaded, String> {
    let path = super::references::relative_path(supplied, workspace.root())?;
    let metadata = workspace
        .perform("ssmcp.fs.stat", json!({"path":path}))
        .await?;
    if metadata["kind"] != "file" {
        return Err("Attach a readable regular workspace file".into());
    }
    if metadata["size"].as_u64().unwrap_or(u64::MAX) > 64 * 1024 * 1024 {
        return Err("File references are limited to 64 MiB".into());
    }
    if metadata["modified_ns"].is_null() {
        return Err("The workspace could not verify this file's modification time".into());
    }
    let first = workspace
        .perform("ssmcp.fs.read", json!({"path":path,"max_bytes":4096}))
        .await?;
    let mut bytes = decode_page(&first)?;
    let image = crate::output::image_mime_type(&bytes).is_some()
        || Path::new(&path)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp"
                )
            });
    if !image {
        return Ok(Loaded::Reference {
            reference: quoted(&path),
            description: format!(
                "File reference ready · {} bytes",
                metadata["size"].as_u64().unwrap_or(0)
            ),
        });
    }
    if metadata["size"].as_u64().unwrap_or(u64::MAX) > MAX_SOURCE_BYTES as u64 {
        return Err("Image sources are limited to 16 MiB before processing".into());
    }
    let mut eof = first["eof"] == true;
    while !eof {
        let page = workspace
            .perform(
                "ssmcp.fs.read",
                json!({"path":path,"offset":bytes.len(),
            "max_bytes":crate::workspace::MAX_FILE_BYTES}),
            )
            .await?;
        let next = decode_page(&page)?;
        if next.is_empty() || bytes.len().saturating_add(next.len()) > MAX_SOURCE_BYTES {
            return Err("The image changed or exceeded its 16 MiB source limit".into());
        }
        bytes.extend_from_slice(&next);
        eof = page["eof"] == true;
    }
    let current = workspace
        .perform("ssmcp.fs.stat", json!({"path":path}))
        .await?;
    if current["kind"] != metadata["kind"]
        || current["size"] != metadata["size"]
        || current["modified_ns"] != metadata["modified_ns"]
        || bytes.len() as u64 != metadata["size"].as_u64().unwrap_or(0)
    {
        return Err("The source image changed while reading it; retry /attach".into());
    }
    store_image(workspace.root().to_path_buf(), path, String::new(), bytes).await
}
fn decode_page(page: &Value) -> Result<Vec<u8>, String> {
    STANDARD
        .decode(
            page["data_base64"]
                .as_str()
                .ok_or("File read returned no bytes")?,
        )
        .map_err(|_| "File read returned invalid base64".into())
}
#[derive(Debug)]
struct PreparedImage {
    label: String,
    mime_type: &'static str,
    bytes: Vec<u8>,
}
async fn store_image(
    root: std::path::PathBuf,
    name: String,
    declared: String,
    bytes: Vec<u8>,
) -> Result<Loaded, String> {
    tokio::task::spawn_blocking(move || {
        let image = prepare(&name, &declared, bytes, MAX_IMAGE_BYTES)?;
        let reference =
            super::references::paste::store_image_at(&root, &image.bytes, image.mime_type)?;
        Ok(Loaded::Reference {
            reference,
            description: format!(
                "{} · {} bytes · {}",
                image.label,
                image.bytes.len(),
                image.mime_type
            ),
        })
    })
    .await
    .map_err(|error| format!("Image processing failed: {error}"))?
}

fn reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("Cannot inspect image: {error}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader)
}
fn mime(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}
fn display_name(
    name: &str,
    dimensions: (u32, u32),
    original: (u32, u32),
    converted: bool,
) -> String {
    let mut base = String::new();
    for ch in Path::new(name)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
    {
        if ch.is_control() {
            continue;
        }
        if base.len() + ch.len_utf8() > 48 {
            break;
        }
        base.push(ch);
    }
    if base.is_empty() {
        base = "clipboard".into();
    }
    if dimensions != original {
        format!(
            "{base} [{}×{} ← {}×{}; resized]",
            dimensions.0, dimensions.1, original.0, original.1
        )
    } else {
        format!(
            "{base} [{}×{}{}]",
            dimensions.0,
            dimensions.1,
            if converted { "; re-encoded" } else { "" }
        )
    }
}

/// Retain small images; large PNGs keep alpha and PNG encoding. Changes are disclosed.
fn prepare(
    name: &str,
    declared_mime: &str,
    bytes: Vec<u8>,
    budget: usize,
) -> Result<PreparedImage, String> {
    if bytes.is_empty() || bytes.len() > MAX_SOURCE_BYTES {
        return Err("Image must contain 1 byte to 16 MiB".into());
    }
    let probe = reader(&bytes)?;
    let format = probe
        .format()
        .ok_or("Clipboard does not contain a recognized image")?;
    let input_mime = mime(format).ok_or("Supported images: PNG, JPEG, GIF and WebP")?;
    if !declared_mime.is_empty() && declared_mime != input_mime {
        return Err("Clipboard image type does not match its contents".into());
    }
    let original = probe
        .into_dimensions()
        .map_err(|error| format!("Cannot inspect image dimensions: {error}"))?;
    if original.0 == 0
        || original.1 == 0
        || original.0 > MAX_DIMENSION
        || original.1 > MAX_DIMENSION
        || u64::from(original.0) * u64::from(original.1) > MAX_PIXELS
    {
        return Err(
            "Image exceeds 32 megapixels or 16384 pixels on one side; crop it first".into(),
        );
    }
    let limit = budget.min(MAX_IMAGE_BYTES);
    if limit < 256 {
        return Err("Image processing budget is too small".into());
    }
    let original_image = reader(&bytes)?
        .decode()
        .map_err(|error| format!("Cannot decode image within the memory limit: {error}"))?;
    let largest = original.0.max(original.1);
    let mut dimensions = if largest > 4096 {
        (
            (u64::from(original.0) * 4096 / u64::from(largest)).max(1) as u32,
            (u64::from(original.1) * 4096 / u64::from(largest)).max(1) as u32,
        )
    } else {
        original
    };
    if bytes.len() <= limit && dimensions == original {
        return Ok(PreparedImage {
            label: display_name(name, original, original, false),
            mime_type: input_mime,
            bytes,
        });
    }
    let jpeg = matches!(format, ImageFormat::Jpeg | ImageFormat::WebP)
        && !original_image.color().has_alpha();
    let output_mime = if jpeg { "image/jpeg" } else { "image/png" };
    for _ in 0..32 {
        let resized;
        let image = if dimensions == original {
            &original_image
        } else {
            resized = original_image.resize_exact(
                dimensions.0,
                dimensions.1,
                image::imageops::FilterType::Lanczos3,
            );
            &resized
        };
        if let Some(encoded) = encode(image, jpeg, limit)? {
            let mut label = display_name(name, dimensions, original, true);
            if matches!(format, ImageFormat::Gif | ImageFormat::WebP) {
                label.push_str(" · static first frame");
            }
            return Ok(PreparedImage {
                label,
                mime_type: output_mime,
                bytes: encoded,
            });
        }
        if dimensions == (1, 1) {
            break;
        }
        dimensions = ((dimensions.0 * 4 / 5).max(1), (dimensions.1 * 4 / 5).max(1));
    }
    Err("Cannot fit this image within 400 KiB; crop it before importing".into())
}
struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}
impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::other("encoded image exceeds its budget"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode(image: &DynamicImage, jpeg: bool, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut output = LimitedBuffer {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    let result = if jpeg {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 90).encode_image(image)
    } else {
        image.write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
            &mut output,
            image::codecs::png::CompressionType::Best,
            image::codecs::png::FilterType::Adaptive,
        ))
    };
    if output.exceeded {
        return Ok(None);
    }
    result.map_err(|error| format!("Cannot encode image: {error}"))?;
    Ok(Some(output.bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_fn(width, height, |x, y| {
            let n = x
                .wrapping_mul(1664525)
                .wrapping_add(y.wrapping_mul(1013904223));
            image::Rgba([
                (n >> 8) as u8,
                (n >> 16) as u8,
                n as u8,
                if x % 2 == 0 { 80 } else { 255 },
            ])
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }
    #[cfg(unix)]
    struct FileFixture(PathBuf);
    #[cfg(unix)]
    impl FileFixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ssmcp-file-batch-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    #[cfg(unix)]
    impl Drop for FileFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_batch_keeps_images_and_text_in_the_same_private_reference_transaction() {
        let temp = FileFixture::new();
        let root = temp.0.join("workspace");
        std::fs::create_dir(&root).unwrap();
        let source = temp.0.join("原文.txt");
        let picture = temp.0.join("图.png");
        let text = "原文保留，不执行任何命令\n";
        let image_bytes = png(4, 4);
        std::fs::write(&source, text).unwrap();
        std::fs::write(&picture, &image_bytes).unwrap();
        let Loaded::Files {
            text: references,
            mut files,
            ..
        } = load_files_at(&root, &[source.clone(), picture.clone()]).unwrap()
        else {
            panic!()
        };
        assert_eq!(files.entries.len(), 2);
        assert!(references.contains("@\".ssmcp/inputs/"));
        for entry in &files.entries {
            assert_eq!(
                std::fs::read(root.join(&entry.path)).unwrap(),
                if entry.name == "原文.txt" {
                    text.as_bytes().to_vec()
                } else {
                    image_bytes.clone()
                }
            );
        }
        let workspace = Workspace::new(root.clone()).unwrap();
        let image = files
            .entries
            .iter()
            .find(|entry| entry.name == "图.png")
            .unwrap();
        let reply = workspace
            .perform("ssmcp.fs.read_image", json!({"path":image.path}))
            .await
            .unwrap();
        assert_eq!(reply["structuredContent"]["mime_type"], "image/png");
        files.keep();
        drop(files);
        assert_eq!(std::fs::read_to_string(&source).unwrap(), text);
        assert_eq!(std::fs::read(&picture).unwrap(), image_bytes);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attached_input_scope_imports_without_a_runtime_or_harness_access() {
        // Runtime::new binds a process global during other unit tests. Run this
        // contract in its own process so absence of that capability is real.
        const CHILD: &str = "SSMCP_ATTACH_INPUT_SCOPE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "client::attachments::tests::attached_input_scope_imports_without_a_runtime_or_harness_access",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        assert!(super::super::interface::harness::local_runtime().is_err());
        let temp = FileFixture::new();
        assert_ne!(std::env::current_dir().unwrap(), temp.0);
        let input = LocalInput::bind(temp.0.clone()).unwrap();
        let held_workspace = input.workspace.clone();
        assert!(LocalInput::bind(temp.0.clone()).is_err());
        assert!(super::super::interface::harness::local_runtime().is_err());
        assert!(
            Task::start(Source::Context {
                command: "context".into(),
                arguments: String::new(),
            })
            .is_err()
        );
        assert!(Task::start(Source::Handoff("scope must not grant this".into())).is_err());

        let bytes = png(8, 8);
        let mut task = Task::start(Source::Image(bytes.clone())).unwrap();
        let loaded = tokio::time::timeout(std::time::Duration::from_secs(5), &mut task.receiver)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Loaded::Reference { reference, .. } = loaded else {
            panic!("attached image must become a private reference");
        };
        let path = reference
            .strip_prefix("@\"")
            .unwrap()
            .strip_suffix('"')
            .unwrap();
        assert_eq!(std::fs::read(temp.0.join(path)).unwrap(), bytes);
        assert_eq!(held_workspace.root(), temp.0.as_path());
        assert!(super::super::interface::harness::local_runtime().is_err());

        drop(task);
        drop(input);
        // Revocation is explicit even if another owner still holds the workspace.
        assert!(Task::start(Source::Image(bytes)).is_err());
        assert!(super::super::interface::harness::local_runtime().is_err());
        drop(held_workspace);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_paste_stores_original_png_and_reads_native_image_content() {
        use std::os::unix::fs::PermissionsExt;
        let temp = FileFixture::new();
        let workspace = Workspace::new(temp.0.clone()).unwrap();
        let bytes = png(8, 8);
        let source = paste_source(bytes.clone()).unwrap();
        let Loaded::Reference { reference, .. } = load(source, &workspace).await.unwrap() else {
            panic!("binary image must become a private image reference");
        };
        let path = reference
            .strip_prefix("@\"")
            .unwrap()
            .strip_suffix('"')
            .unwrap();
        assert_eq!(std::fs::read(temp.0.join(path)).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(temp.0.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let reply = workspace
            .perform("ssmcp.fs.read_image", json!({"path": path}))
            .await
            .unwrap();
        let block = reply["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "image")
            .unwrap();
        assert_eq!(block["mimeType"], "image/png");
        assert_eq!(
            STANDARD.decode(block["data"].as_str().unwrap()).unwrap(),
            bytes
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_binary_paste_creates_no_private_image() {
        let temp = FileFixture::new();
        let workspace = Workspace::new(temp.0.clone()).unwrap();
        let bytes = b"\x89PNG\r\n\x1a\ninvalid".to_vec();
        assert!(
            load(paste_source(bytes).unwrap(), &workspace)
                .await
                .is_err()
        );
        assert!(!temp.0.join(".ssmcp").exists());
    }

    #[test]
    fn paste_classification_preserves_text_and_rejects_unknown_binary_or_overflow() {
        let text = "原稿\n/paste\t保持";
        assert!(matches!(
            paste_source(text.as_bytes().to_vec()).unwrap(),
            Source::Text(value) if value == text
        ));
        assert!(paste_source(vec![0xff, 0x00, 0x03]).is_err());
        assert!(
            paste_source(vec![
                b'x';
                super::super::references::paste::MAX_PASTE_BYTES + 1
            ])
            .is_err()
        );
        let mut image = vec![0; MAX_SOURCE_BYTES + 1];
        image[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(paste_source(image).is_err());
    }

    #[test]
    fn damaged_image_or_nul_paste_is_never_imported_as_text() {
        for bytes in [
            b"plain\0binary".as_slice(),
            b"\xef\xbf\xbdPNG\r\n\x1a\n",
            b"\xc2\x89PNG\r\n\x1a\n",
            b"PNG damaged IHDR",
            b"\xef\xbf\xbd\xef\xbf\xbd broken JFIF",
            b"\xef\xbf\xbd\xef\xbf\xbd broken Exif",
            b"RIFF damaged WEBP",
        ] {
            assert!(paste_source(bytes.to_vec()).is_err(), "{bytes:?}");
        }
        assert!(matches!(
            paste_source("ordinary replacement � is valid text".as_bytes().to_vec()).unwrap(),
            Source::Text(_)
        ));
        let png = png(1, 1);
        for split in 0..=16 {
            assert_eq!(paste_limit(&png[..split], &png[split..]), MAX_SOURCE_BYTES);
        }
    }

    #[cfg(unix)]
    #[test]
    fn invalid_image_rejects_entire_file_batch_before_any_private_copy() {
        let temp = FileFixture::new();
        let root = temp.0.join("workspace");
        std::fs::create_dir(&root).unwrap();
        let source = temp.0.join("good.txt");
        let bad = temp.0.join("bad.png");
        std::fs::write(&source, "unchanged").unwrap();
        // A recognized signature with an invalid body must fail decoding.
        std::fs::write(&bad, [137, 80, 78, 71, 13, 10, 26, 10]).unwrap();
        assert!(load_files_at(&root, &[source.clone(), bad]).is_err());
        assert!(!root.join(".ssmcp").exists());
        assert_eq!(std::fs::read_to_string(source).unwrap(), "unchanged");
    }

    #[test]
    fn explicit_paths_preserve_quotes_without_shell_expansion() {
        assert_eq!(
            path_argument("'docs/设计 图.png'").unwrap(),
            "docs/设计 图.png"
        );
        #[cfg(windows)]
        assert_eq!(
            path_argument("file:///C:/temp/image%20one.png").unwrap(),
            r"C:\temp\image one.png"
        );
        #[cfg(not(windows))]
        assert_eq!(
            path_argument("file:///tmp/image%20one.png").unwrap(),
            "/tmp/image one.png"
        );
        assert_eq!(
            path_argument("'$(touch secret).png'").unwrap(),
            "$(touch secret).png"
        );
        for bad in ["", "one.png two.png", "'unfinished", "path\nname"] {
            assert!(path_argument(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn small_images_are_preserved_and_mime_mismatches_fail() {
        let bytes = png(8, 8);
        let image = prepare("example.png", "image/png", bytes.clone(), MAX_IMAGE_BYTES).unwrap();
        assert_eq!(image.bytes, bytes);
        assert_eq!(image.mime_type, "image/png");
        assert!(image.label.contains("8×8"));
        assert!(prepare("bad.png", "image/jpeg", bytes, MAX_IMAGE_BYTES).is_err());
        assert!(
            prepare(
                "bad.png",
                "image/png",
                b"not an image".to_vec(),
                MAX_IMAGE_BYTES
            )
            .is_err()
        );
    }
    #[test]
    fn oversize_png_preserves_alpha_and_discloses_resize_within_raw_budget() {
        let image = prepare("screenshot.png", "image/png", png(320, 240), 9 * 1024).unwrap();
        assert!(image.bytes.len() <= 9 * 1024);
        assert_eq!(image.mime_type, "image/png");
        let decoded = reader(&image.bytes).unwrap().decode().unwrap();
        assert!(decoded.color().has_alpha());
        assert!(decoded.width() < 320 && decoded.height() < 240);
        assert!(image.label.contains("resized"));
        assert!(
            image
                .label
                .contains(&format!("{}×{}", decoded.width(), decoded.height()))
        );
        assert!(prepare("full.png", "image/png", png(1, 1), 0).is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn attachment_uses_private_inputs_and_rejects_workspace_escape() {
        use std::fs;
        struct Temporary(std::path::PathBuf);
        impl Drop for Temporary {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let root = Temporary(std::env::temp_dir().join(
            format!("ssmcp-attach-{}-{}",std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()),
        ));
        fs::create_dir(&root.0).unwrap();
        fs::write(root.0.join("图.png"), png(4, 4)).unwrap();
        fs::write(root.0.join("note.txt"), "plain fixture").unwrap();
        let workspace = Workspace::new(root.0.clone()).unwrap();
        let Loaded::Reference { reference, .. } = load_path(&workspace, "图.png").await.unwrap()
        else {
            panic!()
        };
        let path = reference
            .strip_prefix("@\"")
            .unwrap()
            .strip_suffix('"')
            .unwrap();
        assert!(path.starts_with(".ssmcp/inputs/image-"));
        let image = workspace
            .perform("ssmcp.fs.read_image", json!({"path":path}))
            .await
            .unwrap();
        assert_eq!(image["structuredContent"]["mime_type"], "image/png");
        let Loaded::Reference { reference, .. } = load_path(&workspace, "note.txt").await.unwrap()
        else {
            panic!()
        };
        assert_eq!(reference, "@\"note.txt\"");
        assert!(load_path(&workspace, "../outside.png").await.is_err());
        std::os::unix::fs::symlink(root.0.join("图.png"), root.0.join("link.png")).unwrap();
        assert!(load_path(&workspace, "link.png").await.is_err());
    }
}
