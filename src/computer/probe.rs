//! Bounded literal-argv calls to the selected standalone executable.
use super::{archive::MAX_EXECUTABLE, release, storage};
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// A broken first entry is a configuration error. Falling through would run a different CLI.
pub(super) fn path_entry(search: Option<&OsStr>) -> Result<Option<PathBuf>, String> {
    let Some(search) = search else {
        return Ok(None);
    };
    #[cfg(not(windows))]
    let names = ["zuse"];
    #[cfg(windows)]
    let names = ["zuse.exe", "zuse.com", "zuse.cmd", "zuse.bat"];
    for dir in std::env::split_paths(search) {
        for name in names {
            let path = dir.join(name);
            match path.symlink_metadata() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("Cannot inspect Zuse entry {}: {e}", path.display())),
                Ok(_) if !crate::platform::is_executable(&path) => {
                    return Err(format!(
                        "Zuse entry is broken or not executable: {}",
                        path.display()
                    ));
                }
                Ok(_) => {
                    let selected = crate::program::selected_entry(&path, "zuse")?;
                    #[cfg(windows)]
                    if !selected
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
                    {
                        return Err("Standalone Zuse requires a native .exe on Windows".into());
                    }
                    return Ok(Some(selected));
                }
            }
        }
    }
    Ok(None)
}

async fn read(pipe: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    pipe.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("exceeded its output limit".into());
    }
    Ok(bytes)
}

pub(super) async fn capture(
    path: &Path,
    args: &[&str],
    input: Option<&str>,
    limit: usize,
    deadline: Duration,
    stopped: impl Future<Output = String>,
) -> Result<Vec<u8>, String> {
    tokio::pin!(stopped);
    tokio::select! {
        biased;
        why = &mut stopped => return Err(why),
        _ = std::future::ready(()) => {}
    }
    let mut cmd = tokio::process::Command::new(path);
    cmd.args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = crate::backend::process::Managed::spawn(&mut cmd)
        .map_err(|e| format!("Could not run the standalone Zuse executable: {e}"))?;
    let stdout = process.child.stdout.take().expect("piped stdout");
    let stderr = process.child.stderr.take().expect("piped stderr");
    let stdin = process.child.stdin.take();
    let exchange = async {
        let write = async move {
            let result = match (stdin, input) {
                (Some(mut pipe), Some(text)) => pipe.write_all(text.as_bytes()).await,
                _ => Ok(()),
            };
            // A CLI that rejects its argv may exit before it reads stdin. Its exit
            // status and diagnostic take precedence over our resulting broken pipe.
            Ok::<_, String>(result)
        };
        let (out, err, status, written) = tokio::try_join!(
            read(stdout, limit),
            read(stderr, 4096),
            async { process.wait().await.map_err(|e| e.to_string()) },
            write
        )?;
        if !status.success() {
            // stdout may include unrelated target inventory from browser.connect.
            // Only expose the CLI's diagnostic, never the completed intermediate results.
            let error = [&err, &out].iter().find_map(|bytes| {
                let data: Value = serde_json::from_slice(bytes).ok()?;
                data["error"].as_str().map(|s| {
                    s.chars()
                        .filter(|c| !c.is_control())
                        .take(512)
                        .collect::<String>()
                })
            });
            let diagnostic = error.or_else(|| {
                (args == ["--version"] && !err.is_empty()).then(|| {
                    String::from_utf8_lossy(&err)
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(512)
                        .collect()
                })
            });
            return Err(format!(
                "failed ({status}): {}",
                diagnostic.as_deref().unwrap_or("no structured diagnostic")
            ));
        }
        written.map_err(|error| format!("writing stdin failed: {error}"))?;
        Ok(out)
    };
    let outcome = tokio::select! {
        biased;
        why = &mut stopped => Err(why),
        _ = tokio::time::sleep(deadline) => Err("exceeded its deadline".into()),
        result = exchange => result,
    };
    process.shutdown().await;
    outcome.map_err(|error| format!("Zuse {} {error}", args[0]))
}

pub(super) async fn version(
    path: &Path,
    stopped: impl Future<Output = String>,
) -> Result<Value, String> {
    let (before, bytes) = storage::bytes(path, MAX_EXECUTABLE)?;
    let sha256 = release::digest(&bytes);
    let size = bytes.len();
    drop(bytes);
    let out = capture(
        path,
        &["--version"],
        None,
        4096,
        Duration::from_secs(3),
        stopped,
    )
    .await?;
    let text = std::str::from_utf8(&out).map_err(|e| e.to_string())?.trim();
    let value = text
        .strip_prefix("zuse ")
        .ok_or("Zuse --version returned an unexpected identity")?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    {
        return Err("Zuse --version returned an invalid version token".into());
    }
    let (after, current) = storage::bytes(path, MAX_EXECUTABLE)?;
    if before != after || sha256 != release::digest(&current) {
        return Err("Zuse executable changed during its version query".into());
    }
    Ok(json!({"available": true, "version": value, "path": path,
        "sha256": sha256, "size": size, "queried": "--version"}))
}
