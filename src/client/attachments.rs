//! Bounded image/file loading for the local composer, outside the display lock.
use super::references::files;
use crate::wire::{MAX_USER_IMAGE_BYTES, MAX_USER_IMAGES, MAX_USER_IMAGES_BYTES, UserImage};
use image::{DynamicImage, ImageFormat, ImageReader};
use serde_json::{Value, json};
use std::{
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};
use tokio::{sync::oneshot, task::JoinHandle};

pub(super) const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_DIMENSION: u32 = 16_384;

pub(super) enum Source {
    Clipboard,
    Paths(Vec<PathBuf>),
    Files(Vec<PathBuf>),
    Copy(String),
}
#[derive(Debug)]
pub(super) enum Loaded {
    Text(String),
    Images(Vec<UserImage>),
    Files {
        text: String,
        images: Vec<UserImage>,
        files: files::ImportedFiles,
    },
    Copied,
}

pub(super) struct Task {
    receiver: oneshot::Receiver<Result<Loaded, String>>,
    handle: JoinHandle<()>,
    pub fallback: Option<String>,
    pub copying: bool,
}
impl Task {
    pub(super) fn start(
        source: Source,
        current: &[UserImage],
        fallback: Option<String>,
    ) -> Result<Self, String> {
        let copying = matches!(&source, Source::Copy(_));
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "Clipboard loading requires the interactive client runtime")?;
        let slots = MAX_USER_IMAGES.saturating_sub(current.len());
        let budget = MAX_USER_IMAGES_BYTES
            .saturating_sub(current.iter().map(|image| image.data.len()).sum::<usize>());
        let (sender, receiver) = oneshot::channel();
        let handle = runtime.spawn(async move {
            let result = match source {
                Source::Copy(text) => super::clipboard::write_text(&text)
                    .await
                    .map(|()| Loaded::Copied),
                Source::Clipboard => match super::clipboard::read().await {
                    Ok(super::clipboard::Content::Image {
                        name,
                        mime_type,
                        bytes,
                    }) => tokio::task::spawn_blocking(move || {
                        if slots == 0 {
                            return Err("At most four images per request; use /detach first".into());
                        }
                        prepare(&name, &mime_type, bytes, budget.min(MAX_USER_IMAGE_BYTES))
                            .map(|image| Loaded::Images(vec![image]))
                    })
                    .await
                    .unwrap_or_else(|error| Err(format!("Image processing failed: {error}"))),
                    Ok(super::clipboard::Content::Text(text)) => {
                        if let Some(paths) = pasted_paths(&text) {
                            tokio::task::spawn_blocking(move || {
                                match load_paths(&paths, slots, budget) {
                                    Ok(images) => Ok(Loaded::Images(images)),
                                    // Text resembling a missing image path must remain ordinary text.
                                    Err(_) => Ok(Loaded::Text(text)),
                                }
                            })
                            .await
                            .unwrap_or_else(|error| Err(format!("Image loading failed: {error}")))
                        } else if let Some(paths) = pasted_file_paths(&text) {
                            tokio::task::spawn_blocking(move || load_files(&paths, slots, budget))
                                .await
                                .unwrap_or_else(|error| Err(format!("File import failed: {error}")))
                        } else {
                            Ok(Loaded::Text(text))
                        }
                    }
                    #[cfg(all(not(target_os = "macos"), not(windows)))]
                    Ok(super::clipboard::Content::Files(paths)) => {
                        tokio::task::spawn_blocking(move || load_files(&paths, slots, budget))
                            .await
                            .unwrap_or_else(|error| Err(format!("File import failed: {error}")))
                    }
                    Err(error) => Err(error),
                },
                Source::Paths(paths) => tokio::task::spawn_blocking(move || {
                    load_paths(&paths, slots, budget).map(Loaded::Images)
                })
                .await
                .unwrap_or_else(|error| Err(format!("Image loading failed: {error}"))),
                Source::Files(paths) => {
                    tokio::task::spawn_blocking(move || load_files(&paths, slots, budget))
                        .await
                        .unwrap_or_else(|error| Err(format!("File import failed: {error}")))
                }
            };
            let _ = sender.send(result);
        });
        Ok(Self {
            receiver,
            handle,
            fallback,
            copying,
        })
    }
    pub(super) fn poll(&mut self) -> Option<Result<Loaded, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err("Image loading stopped".into())),
        }
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub(super) fn metadata(images: &[UserImage]) -> Vec<Value> {
    images
        .iter()
        .map(|image| {
            json!({
                "id": image.id, "name": image.name, "mime_type": image.mime_type,
                "encoded_bytes": image.data.len(), "bytes": decoded_bytes(&image.data),
            })
        })
        .collect()
}
pub(super) fn description(image: &UserImage) -> String {
    format!(
        "{} · {} · {} KiB",
        image.name,
        image
            .mime_type
            .strip_prefix("image/")
            .unwrap_or(&image.mime_type)
            .to_uppercase(),
        decoded_bytes(&image.data).div_ceil(1024)
    )
}
fn decoded_bytes(data: &str) -> usize {
    (data.len() / 4 * 3).saturating_sub(data.bytes().rev().take_while(|byte| *byte == b'=').count())
}

/// Parse complete image paths with quotes/escaped spaces, never shell expansion.
pub(super) fn pasted_paths(text: &str) -> Option<Vec<PathBuf>> {
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
        return None;
    }
    if !part.is_empty() {
        parts.push(part);
    }
    if parts.is_empty() || parts.len() > MAX_USER_IMAGES {
        return None;
    }
    parts
        .into_iter()
        .map(|part| {
            let path = if part.starts_with("file://") {
                reqwest::Url::parse(&part).ok()?.to_file_path().ok()?
            } else if let Some(rest) = part.strip_prefix("~/") {
                let directory =
                    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
                PathBuf::from(directory).join(rest)
            } else {
                PathBuf::from(part)
            };
            let extension = path.extension()?.to_str()?.to_ascii_lowercase();
            matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp").then_some(path)
        })
        .collect()
}

pub(super) fn pasted_file_paths(text: &str) -> Option<Vec<PathBuf>> {
    files::pasted_paths(text)
}

fn load_files(paths: &[PathBuf], slots: usize, budget: usize) -> Result<Loaded, String> {
    let root = std::env::current_dir()
        .map_err(|error| format!("Cannot locate the workspace for pasted files: {error}"))?;
    load_files_at(&root, paths, slots, budget)
}

fn load_files_at(
    root: &Path,
    paths: &[PathBuf],
    slots: usize,
    budget: usize,
) -> Result<Loaded, String> {
    fn image_type(file: &files::SelectedFile) -> Option<&'static str> {
        crate::output::image_mime_type(&file.bytes).filter(|mime| {
            matches!(
                *mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        })
    }
    let selected = files::read(paths)?;
    let image_count = selected
        .iter()
        .filter(|file| image_type(file).is_some())
        .count();
    if image_count > slots {
        return Err("At most four images per request; use /detach first".into());
    }
    let per_image = (budget / image_count.max(1)).min(MAX_USER_IMAGE_BYTES);
    let mut images = Vec::with_capacity(image_count);
    let mut ordinary = Vec::new();
    for file in selected {
        if let Some(mime) = image_type(&file) {
            images.push(prepare(&file.name, mime, file.bytes, per_image)?);
        } else {
            ordinary.push(file);
        }
    }
    if ordinary.is_empty() {
        return Ok(Loaded::Images(images));
    }
    // Decode every selected image and validate every source before publishing any copy.
    let files = files::import(root, ordinary)?;
    let text = files.references();
    Ok(Loaded::Files {
        text,
        images,
        files,
    })
}

fn load_paths(paths: &[PathBuf], slots: usize, budget: usize) -> Result<Vec<UserImage>, String> {
    if paths.is_empty() || paths.len() > slots {
        return Err("At most four images per request; use /detach first".into());
    }
    let per_image = (budget / paths.len()).min(MAX_USER_IMAGE_BYTES);
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("Cannot open image {}: {error}", path.display()))?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES as u64 {
            return Err("Attach a regular image file of at most 16 MiB".into());
        }
        let mut bytes = Vec::new();
        file.take((MAX_SOURCE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err("Image exceeds the 16 MiB source limit".into());
        }
        images.push(prepare(
            &path.file_name().unwrap_or_default().to_string_lossy(),
            "",
            bytes,
            per_image,
        )?);
    }
    Ok(images)
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
    encoded_budget: usize,
) -> Result<UserImage, String> {
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
    let limit = encoded_budget.min(MAX_USER_IMAGE_BYTES) / 4 * 3;
    if limit < 256 {
        return Err("Image attachment budget is full; use /detach first".into());
    }
    let original_image = reader(&bytes)?
        .decode()
        .map_err(|error| format!("Cannot decode image within the memory limit: {error}"))?;
    if bytes.len() <= limit {
        return UserImage::from_bytes(
            &display_name(name, original, original, false),
            input_mime,
            &bytes,
        );
    }
    let jpeg = matches!(format, ImageFormat::Jpeg | ImageFormat::WebP)
        && !original_image.color().has_alpha();
    let output_mime = if jpeg { "image/jpeg" } else { "image/png" };
    let mut dimensions = original;
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
            return UserImage::from_bytes(
                &display_name(name, dimensions, original, true),
                output_mime,
                &encoded,
            );
        }
        if dimensions == (1, 1) {
            break;
        }
        dimensions = ((dimensions.0 * 4 / 5).max(1), (dimensions.1 * 4 / 5).max(1));
    }
    Err("Cannot fit this image in the attachment budget; crop it or remove another image".into())
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
    use base64::Engine as _;
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
    #[test]
    fn path_pastes_parse_quotes_without_running_shell_expressions() {
        assert_eq!(
            pasted_paths("'/tmp/image one.png' /tmp/two.jpeg").unwrap(),
            vec![
                PathBuf::from("/tmp/image one.png"),
                PathBuf::from("/tmp/two.jpeg")
            ]
        );
        assert_eq!(
            pasted_paths("file:///tmp/image%20one.png").unwrap(),
            vec![PathBuf::from("/tmp/image one.png")]
        );
        for text in [
            "Please inspect image.png",
            "/tmp/one.txt",
            "'/tmp/unfinished.png",
            "/tmp/$(touch secret).png",
        ] {
            assert!(pasted_paths(text).is_none());
        }
    }
    #[test]
    fn small_images_are_retained_and_metadata_never_contains_payload() {
        let bytes = png(8, 8);
        let attached = prepare(
            "example.png",
            "image/png",
            bytes.clone(),
            MAX_USER_IMAGE_BYTES,
        )
        .unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&attached.data)
                .unwrap(),
            bytes
        );
        assert!(attached.name.contains("8×8"));
        let metadata = metadata(&[attached]);
        assert!(metadata[0].get("data").is_none());
        assert_eq!(metadata[0]["mime_type"], "image/png");
    }
    #[test]
    fn oversized_pngs_are_resized_with_alpha_and_report_actual_dimensions() {
        let attached = prepare("screenshot.png", "image/png", png(320, 240), 12 * 1024).unwrap();
        assert!(attached.data.len() <= 12 * 1024);
        assert_eq!(attached.mime_type, "image/png");
        assert!(attached.name.contains("resized"));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&attached.data)
            .unwrap();
        let decoded = reader(&bytes).unwrap().decode().unwrap();
        assert!(decoded.color().has_alpha());
        assert!(decoded.width() < 320 && decoded.height() < 240);
        assert!(
            attached
                .name
                .contains(&format!("{}×{}", decoded.width(), decoded.height()))
        );
    }
    #[test]
    fn invalid_images_and_full_budgets_leave_no_partial_attachment() {
        assert!(
            prepare(
                "bad.png",
                "image/png",
                b"not an image".to_vec(),
                MAX_USER_IMAGE_BYTES
            )
            .is_err()
        );
        assert!(prepare("bad.png", "image/jpeg", png(1, 1), MAX_USER_IMAGE_BYTES).is_err());
        assert!(prepare("image.png", "image/png", png(1, 1), 0).is_err());
    }
}
