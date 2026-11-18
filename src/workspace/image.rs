//! Complete raster reads through the same root-scoped handles as other file tools.
use super::*;
use crate::output::{MAX_IMAGE_BYTES, image_mime_type};

impl Root {
    pub(super) fn read_image(&self, args: PathArgs) -> Result<Value, String> {
        let mut file = self.file(&args.path)?;
        let before = regular_metadata(&file)?;
        let too_large =
            || format!("image exceeds 400 KiB ({MAX_IMAGE_BYTES} bytes); use a smaller image");
        if before.len() > MAX_IMAGE_BYTES as u64 {
            return Err(too_large());
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(too_large());
        }
        if bytes.len() as u64 != before.len() || !same_version(&before, &regular_metadata(&file)?) {
            return Err("file changed during read; retry from a fresh read".into());
        }
        let mime_type = image_mime_type(&bytes)
            .ok_or("unsupported image header; expected PNG, JPEG, GIF or WebP")?;
        let metadata = json!({
            "path":args.path, "mime_type":mime_type,
            "size":bytes.len(), "sha256":sha256(&bytes)
        });
        Ok(json!({
            "isError":false,
            "content":[
                {"type":"image","mimeType":mime_type,"data":STANDARD.encode(&bytes)},
                {"type":"text","text":metadata.to_string()}
            ],
            "structuredContent":metadata
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("ssmcp-image-test-{}", random_id().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn workspace(&self) -> Workspace {
            Workspace::new(self.0.clone()).unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn image_reads_preserve_bytes_mime_metadata_and_audit() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace();
        // Header fixtures exercise transport, not image decoding.
        let formats: &[(&str, &[u8])] = &[
            ("image/png", b"\x89PNG\r\n\x1a\npayload"),
            ("image/jpeg", &[0xff, 0xd8, 0xff, 0xe0]),
            ("image/gif", b"GIF89apayload"),
            ("image/webp", b"RIFF\x0c\x00\x00\x00WEBPVP8 payload"),
        ];
        for (index, (mime, bytes)) in formats.iter().enumerate() {
            let path = format!("image-{index}.bin");
            std::fs::write(scratch.0.join(&path), bytes).unwrap();
            let result = workspace
                .perform("ssmcp.fs.read_image", json!({"path":path}))
                .await
                .unwrap();
            assert_eq!(result["isError"], false);
            assert_eq!(result["content"].as_array().unwrap().len(), 2);
            let image = &result["content"][0];
            assert_eq!(image["type"], "image");
            assert_eq!(image["mimeType"], *mime);
            assert_eq!(
                STANDARD.decode(image["data"].as_str().unwrap()).unwrap(),
                *bytes
            );
            assert_eq!(
                crate::output::validate_image_content(image).unwrap(),
                bytes.len()
            );
            assert_eq!(
                result["structuredContent"],
                json!({
                    "path":path,"mime_type":mime,"size":bytes.len(),"sha256":sha256(bytes)
                })
            );
            assert_eq!(
                serde_json::from_str::<Value>(result["content"][1]["text"].as_str().unwrap())
                    .unwrap(),
                result["structuredContent"]
            );
        }
        let events = workspace
            .perform("ssmcp.fs.events", json!({}))
            .await
            .unwrap();
        let events = events["events"].as_array().unwrap();
        assert_eq!(events.len(), formats.len());
        for (entry, (_, bytes)) in events.iter().zip(formats) {
            let event = &entry["event"];
            assert_eq!(event["tool"], "ssmcp.fs.read_image");
            assert_eq!(event["bytes"], bytes.len());
            assert_eq!(event["offset"], 0);
            assert!(event.get("data").is_none());
            assert!(event.get("data_base64").is_none());
        }
    }

    #[tokio::test]
    async fn image_size_limit_is_complete_and_survives_the_text_guard() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace();
        let mut bytes = vec![0xab; MAX_IMAGE_BYTES];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        std::fs::write(scratch.0.join("limit.png"), &bytes).unwrap();
        let result = workspace
            .perform("ssmcp.fs.read_image", json!({"path":"limit.png"}))
            .await
            .unwrap();
        assert_eq!(result["structuredContent"]["size"], MAX_IMAGE_BYTES);
        assert_eq!(
            STANDARD
                .decode(result["content"][0]["data"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        let output = crate::output::OutputStore::global().unwrap();
        assert_eq!(output.guard_result(result.clone()), result);
        bytes.push(0xab);
        std::fs::write(scratch.0.join("limit.png"), &bytes).unwrap();
        let error = workspace
            .perform("ssmcp.fs.read_image", json!({"path":"limit.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("400 KiB (409600 bytes)"), "{error}");
        assert!(error.contains("smaller image"), "{error}");
    }

    #[tokio::test]
    async fn image_reads_reject_unsupported_and_outside_paths() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace();
        std::fs::write(scratch.0.join("fake.png"), b"<svg/>").unwrap();
        std::fs::write(scratch.0.join("empty.png"), b"").unwrap();
        std::fs::create_dir(scratch.0.join("directory.png")).unwrap();
        for path in [
            "fake.png",
            "empty.png",
            "directory.png",
            "missing.png",
            "../image.png",
            ".",
        ] {
            assert!(
                workspace
                    .perform("ssmcp.fs.read_image", json!({"path":path}))
                    .await
                    .is_err(),
                "{path}"
            );
        }
        let absolute = scratch.0.join("fake.png").to_string_lossy().into_owned();
        assert!(
            workspace
                .perform("ssmcp.fs.read_image", json!({"path":absolute}))
                .await
                .is_err()
        );
        assert!(
            workspace
                .perform("ssmcp.fs.read_image", json!({"path":"fake.png","offset":1}))
                .await
                .is_err()
        );
        assert_eq!(
            workspace
                .perform("ssmcp.fs.events", json!({}))
                .await
                .unwrap()["next_cursor"],
            0
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn image_reads_reject_symlinks_and_nonregular_files() {
        let scratch = Scratch::new();
        let outside = Scratch::new();
        let workspace = scratch.workspace();
        std::fs::write(outside.0.join("image.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        std::os::unix::fs::symlink(outside.0.join("image.png"), scratch.0.join("link.png"))
            .unwrap();
        std::os::unix::fs::symlink(&outside.0, scratch.0.join("escape")).unwrap();
        let fifo =
            std::ffi::CString::new(scratch.0.join("fifo.png").as_os_str().as_encoded_bytes())
                .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for path in ["link.png", "escape/image.png", "fifo.png"] {
            assert!(
                workspace
                    .perform("ssmcp.fs.read_image", json!({"path":path}))
                    .await
                    .is_err(),
                "{path}"
            );
        }
    }
}
