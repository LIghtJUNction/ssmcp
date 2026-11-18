//! Read the explicitly requested system clipboard; never poll or modify it.
use std::{path::Path, time::Duration};
pub(super) enum Content {
    Image {
        name: String,
        mime_type: String,
        bytes: Vec<u8>,
    },
    Text(String),
}
async fn helper(program: &str, arguments: &[&str], limit: usize) -> Result<Vec<u8>, String> {
    crate::computer::run_helper(
        Path::new("."),
        program,
        &arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>(),
        None,
        limit,
        Duration::from_secs(3),
    )
    .await
}
fn text(bytes: Vec<u8>) -> Result<Content, String> {
    String::from_utf8(bytes)
        .map(Content::Text)
        .map_err(|_| "Clipboard text is not valid UTF-8".into())
}
#[cfg(all(not(target_os = "macos"), not(windows)))]
pub(super) async fn read() -> Result<Content, String> {
    let mut failures = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match read_unix("wl-paste", true).await {
            Ok(content) => return Ok(content),
            Err(error) => failures.push(error),
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        match read_unix("xclip", false).await {
            Ok(content) => return Ok(content),
            Err(error) => failures.push(error),
        }
    }
    Err(if failures.is_empty() {
        "No desktop clipboard is available. Paste text normally or use /attach PATH for an image."
            .into()
    } else {
        format!(
            "Cannot read clipboard; use terminal paste or /attach PATH. {}",
            failures.join("; ")
        )
    })
}
#[cfg(all(not(target_os = "macos"), not(windows)))]
async fn read_unix(program: &str, wayland: bool) -> Result<Content, String> {
    let listing = if wayland {
        vec!["--list-types"]
    } else {
        vec!["-selection", "clipboard", "-o", "-t", "TARGETS"]
    };
    let types = helper(program, &listing, 16 * 1024).await?;
    let types = String::from_utf8(types).map_err(|_| "Clipboard type list is not UTF-8")?;
    for (mime, extension) in [
        ("image/png", "png"),
        ("image/jpeg", "jpg"),
        ("image/webp", "webp"),
        ("image/gif", "gif"),
    ] {
        if types.lines().any(|line| line.trim() == mime) {
            let args = if wayland {
                vec!["--no-newline", "--type", mime]
            } else {
                vec!["-selection", "clipboard", "-o", "-t", mime]
            };
            let bytes = helper(program, &args, super::attachments::MAX_SOURCE_BYTES).await?;
            return Ok(Content::Image {
                name: format!("clipboard.{extension}"),
                mime_type: mime.into(),
                bytes,
            });
        }
    }
    if types.lines().any(|line| line.trim().starts_with("image/")) {
        return Err("Clipboard image format is unsupported; use a PNG/JPEG/GIF/WebP file".into());
    }
    let target = [
        "text/uri-list",
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
    ]
    .into_iter()
    .find(|kind| types.lines().any(|line| line.trim() == *kind))
    .ok_or("Clipboard does not contain an image or plain text")?;
    let args = if wayland {
        vec!["--no-newline", "--type", target]
    } else {
        vec!["-selection", "clipboard", "-o", "-t", target]
    };
    text(helper(program, &args, super::composer::MAX_DRAFT_BYTES).await?)
}
#[cfg(target_os = "macos")]
pub(super) async fn read() -> Result<Content, String> {
    use base64::Engine as _;
    const TYPES: &str = "ObjC.import('AppKit'); (ObjC.deepUnwrap($.NSPasteboard.generalPasteboard.types)||[]).join('\\n');";
    const IMAGE: &str = "ObjC.import('AppKit'); const im=$.NSImage.alloc.initWithPasteboard($.NSPasteboard.generalPasteboard); const rep=$.NSBitmapImageRep.imageRepWithData(im.TIFFRepresentation); ObjC.unwrap(rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG,$({})).base64EncodedStringWithOptions(0));";
    let types = helper("osascript", &["-l", "JavaScript", "-e", TYPES], 16 * 1024).await?;
    let types = String::from_utf8(types).map_err(|_| "Clipboard type list is not UTF-8")?;
    let image_available = types.lines().any(|kind| {
        matches!(
            kind.trim(),
            "public.png"
                | "public.tiff"
                | "public.jpeg"
                | "com.compuserve.gif"
                | "org.webmproject.webp"
        )
    });
    if !image_available {
        return text(
            helper(
                "pbpaste",
                &["-Prefer", "txt"],
                super::composer::MAX_DRAFT_BYTES,
            )
            .await?,
        );
    }
    let encoded = helper(
        "osascript",
        &["-l", "JavaScript", "-e", IMAGE],
        super::attachments::MAX_SOURCE_BYTES * 4 / 3 + 4,
    )
    .await?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim_ascii())
        .map_err(|_| "Cannot decode the clipboard image; use /attach PATH")?;
    if bytes.is_empty() {
        return Err("The clipboard image is empty; use /attach PATH".into());
    }
    Ok(Content::Image {
        name: "clipboard.png".into(),
        mime_type: "image/png".into(),
        bytes,
    })
}
#[cfg(windows)]
pub(super) async fn read() -> Result<Content, String> {
    use base64::Engine as _;
    const IMAGE: &str = "Add-Type -AssemblyName System.Windows.Forms; $i=[System.Windows.Forms.Clipboard]::GetImage(); if($null -ne $i){$m=[IO.MemoryStream]::new(); try{$i.Save($m,[System.Drawing.Imaging.ImageFormat]::Png); [Console]::Write([Convert]::ToBase64String($m.ToArray()))} finally{$m.Dispose();$i.Dispose()}}";
    const TEXT: &str = "Add-Type -AssemblyName System.Windows.Forms; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); [Console]::Write([System.Windows.Forms.Clipboard]::GetText())";
    let encoded = helper(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-STA", "-Command", IMAGE],
        super::attachments::MAX_SOURCE_BYTES * 4 / 3 + 4,
    )
    .await?;
    if !encoded.trim_ascii().is_empty() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim_ascii())
            .map_err(|_| "Cannot decode the clipboard image; use /attach PATH")?;
        return Ok(Content::Image {
            name: "clipboard.png".into(),
            mime_type: "image/png".into(),
            bytes,
        });
    }
    text(
        helper(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-STA", "-Command", TEXT],
            super::composer::MAX_DRAFT_BYTES,
        )
        .await?,
    )
}

const MAX_COPY_BYTES: usize = 1024 * 1024;

/// Native clipboard owners deliberately outlive the short launcher process.
/// Do not use the managed capture helper here: its process-group cleanup would
/// destroy wl-copy/xclip's owner as soon as the write appeared to succeed.
async fn write_owner(mut command: tokio::process::Command, text: &str) -> Result<(), String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Cannot start clipboard writer: {error}"))?;
    let outcome = tokio::time::timeout(Duration::from_secs(3), async {
        let mut input = child
            .stdin
            .take()
            .ok_or("Clipboard writer stdin is unavailable")?;
        input
            .write_all(text.as_bytes())
            .await
            .map_err(|error| format!("Cannot write clipboard: {error}"))?;
        input
            .shutdown()
            .await
            .map_err(|error| format!("Cannot finish clipboard write: {error}"))?;
        drop(input);
        let status = child
            .wait()
            .await
            .map_err(|error| format!("Cannot wait for clipboard writer: {error}"))?;
        if !status.success() {
            return Err(format!(
                "Clipboard writer failed ({status}); check the desktop clipboard is available"
            ));
        }
        Ok(())
    })
    .await;
    match outcome {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            Err("Clipboard writer timed out; the draft is preserved".into())
        }
    }
}
async fn write_using(program: &str, arguments: &[&str], text: &str) -> Result<(), String> {
    let executable = crate::program::executable(program)?;
    let mut command = tokio::process::Command::new(executable);
    command.args(arguments);
    write_owner(command, text).await
}

pub(super) async fn write_text(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("The latest agent message is empty".into());
    }
    if value.len() > MAX_COPY_BYTES {
        return Err("The latest agent message exceeds the 1 MiB clipboard limit".into());
    }
    write_platform(value).await
}
#[cfg(all(not(target_os = "macos"), not(windows)))]
async fn write_platform(value: &str) -> Result<(), String> {
    let mut failures = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match write_using("wl-copy", &["--type", "text/plain;charset=utf-8"], value).await {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(error),
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        match write_using(
            "xclip",
            &["-selection", "clipboard", "-in", "-silent"],
            value,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(error),
        }
    }
    Err(if failures.is_empty() {
        "No desktop clipboard is available for /copy".into()
    } else {
        format!(
            "Cannot copy the agent message; install wl-clipboard (Wayland) or xclip (X11). {}",
            failures.join("; ")
        )
    })
}
#[cfg(target_os = "macos")]
async fn write_platform(value: &str) -> Result<(), String> {
    write_using("pbcopy", &[], value).await
}
#[cfg(windows)]
async fn write_platform(value: &str) -> Result<(), String> {
    const SCRIPT: &str = "Add-Type -AssemblyName System.Windows.Forms; [Console]::InputEncoding=[Text.UTF8Encoding]::new($false); [System.Windows.Forms.Clipboard]::SetText([Console]::In.ReadToEnd())";
    write_using(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-STA", "-Command", SCRIPT],
        value,
    )
    .await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn copy_writer_preserves_exact_text_and_leaves_the_owner_alive() {
        // Isolated fake owner: never reads or modifies the user's clipboard.
        let root = std::env::temp_dir().join(format!(
            "ssmcp-copy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let text_file = root.join("input");
        let owned_file = root.join("owned");
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-c",
                "cat > \"$1\"; (sleep 0.05; printf owned > \"$2\") >/dev/null 2>&1 &",
                "ssmcp-test",
            ])
            .arg(&text_file)
            .arg(&owned_file);
        let text = "中文 agent response\nsecond line\n";
        write_owner(command, text).await.unwrap();
        assert_eq!(std::fs::read_to_string(&text_file).unwrap(), text);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !owned_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&owned_file).unwrap(), "owned");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn copy_writer_reports_failure_instead_of_claiming_success() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "cat >/dev/null; exit 7"]);
        assert!(
            write_owner(command, "agent text")
                .await
                .unwrap_err()
                .contains("failed")
        );
        assert!(write_text("").await.is_err());
        assert!(write_text(&"x".repeat(MAX_COPY_BYTES + 1)).await.is_err());
    }
}
