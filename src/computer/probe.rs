//! The only executable invocation in this module is a bounded `zuse --version`.
use super::{archive::MAX_EXECUTABLE, release, storage};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

const OUTPUT_LIMIT: u64 = 4096;
const DEADLINE: Duration = Duration::from_secs(3);

pub(super) async fn version(path: &Path) -> Result<Value, String> {
    let (before, bytes) = storage::bytes(path, MAX_EXECUTABLE)?;
    let sha256 = release::digest(&bytes);
    let size = bytes.len();
    let mut command = tokio::process::Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = crate::backend::process::Managed::spawn(&mut command)
        .map_err(|error| format!("Could not run the standalone Zuse executable: {error}"))?;
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or("Zuse version stdout is unavailable")?
        .take(OUTPUT_LIMIT + 1);
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or("Zuse version stderr is unavailable")?
        .take(OUTPUT_LIMIT + 1);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let outcome = tokio::time::timeout(DEADLINE, async {
        let (read_out, read_err, status) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            process.wait()
        );
        read_out.map_err(|error| error.to_string())?;
        read_err.map_err(|error| error.to_string())?;
        let status = status.map_err(|error| error.to_string())?;
        if out.len() as u64 > OUTPUT_LIMIT || err.len() as u64 > OUTPUT_LIMIT {
            return Err("Zuse --version exceeded its output limit".to_string());
        }
        if !status.success() {
            return Err(format!(
                "Zuse --version failed ({status}): {}",
                String::from_utf8_lossy(&err)
            ));
        }
        let text = std::str::from_utf8(&out)
            .map_err(|error| error.to_string())?
            .trim();
        let value = text
            .strip_prefix("zuse ")
            .ok_or("Zuse --version returned an unexpected identity")?;
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        {
            return Err("Zuse --version returned an invalid version token".into());
        }
        let version = value.to_owned();
        if text.contains(['\r', '\n']) {
            return Err("Zuse --version returned more than one line".into());
        }
        Ok(version)
    })
    .await
    .map_err(|_| "Zuse --version exceeded its three-second deadline".to_string())
    .and_then(|result| result);
    process.shutdown().await;
    let version = outcome?;
    let (after, current) = storage::bytes(path, MAX_EXECUTABLE)?;
    if before != after || sha256 != release::digest(&current) {
        return Err("Zuse executable changed during its version query".into());
    }
    Ok(json!({
        "available": true, "version": version, "path": path,
        "sha256": sha256, "size": size, "queried": "--version"
    }))
}
