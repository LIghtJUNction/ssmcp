//! Bounded line framing without a blocking stdin thread or shared FD flag changes.
use super::{FRAME_LIMIT, Sender, rpc, rpc_error};
use crate::wire::MAX_USER_MESSAGE_BYTES;
use serde_json::{Value, json};
use std::{io, time::Duration};

const STDIN: libc::c_int = libc::STDIN_FILENO;
const STDOUT: libc::c_int = libc::STDOUT_FILENO;

async fn ready(fd: libc::c_int, events: libc::c_short) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        // SAFETY: the pollfd is valid, and timeout 0 never blocks the runtime.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result > 0 {
            return Ok(());
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

#[derive(Default)]
struct Lines {
    buffered: Vec<u8>,
}

impl Lines {
    fn take_line(&mut self, limit: usize) -> Result<Option<Vec<u8>>, String> {
        if let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
            if end > limit {
                return Err("chat input line exceeds its size limit".into());
            }
            return Ok(Some(self.buffered.drain(..=end).collect()));
        }
        if self.buffered.len() > limit {
            return Err("chat input line exceeds its size limit".into());
        }
        Ok(None)
    }

    async fn next(&mut self, limit: usize, allow_tail: bool) -> Result<Option<Vec<u8>>, String> {
        loop {
            if let Some(line) = self.take_line(limit)? {
                return Ok(Some(line));
            }
            ready(STDIN, libc::POLLIN)
                .await
                .map_err(|error| error.to_string())?;
            let mut bytes = [0u8; 2048];
            // SAFETY: stdin is borrowed, and bytes is valid for up to 2048 bytes.
            // This is the sole reader of redirected stdin after readiness.
            let count = unsafe { libc::read(STDIN, bytes.as_mut_ptr().cast(), bytes.len()) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    continue;
                }
                return Err(error.to_string());
            }
            if count == 0 {
                if self.buffered.is_empty() {
                    return Ok(None);
                }
                if allow_tail {
                    return Ok(Some(std::mem::take(&mut self.buffered)));
                }
                return Err("MCP stdin ended inside a JSON-RPC line".into());
            }
            self.buffered.extend_from_slice(&bytes[..count as usize]);
        }
    }
}

fn encode(value: &Value) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

async fn write(value: Value) -> Result<(), String> {
    let bytes = encode(&value)?;
    let mut offset = 0;
    // Write only small chunks after polling; 512 bytes is within POSIX PIPE_BUF.
    while offset < bytes.len() {
        ready(STDOUT, libc::POLLOUT)
            .await
            .map_err(|error| error.to_string())?;
        let remaining = &bytes[offset..];
        // SAFETY: the checked slice lives for the duration of the borrowed-fd write.
        let count =
            unsafe { libc::write(STDOUT, remaining.as_ptr().cast(), remaining.len().min(512)) };
        if count <= 0 {
            if count == 0 {
                return Err("MCP stdout did not accept bytes".into());
            }
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                continue;
            }
            return Err(error.to_string());
        }
        offset += count as usize;
    }
    Ok(())
}

pub(super) async fn serve(sender: Sender, mcp: bool) -> Result<(), String> {
    let mut lines = Lines::default();
    if mcp {
        return serve_mcp(sender, &mut lines).await;
    }
    while let Some(line) = lines.next(MAX_USER_MESSAGE_BYTES + 1, true).await? {
        let text = std::str::from_utf8(&line).map_err(|_| "chat input must be UTF-8")?;
        let text = text.strip_suffix('\n').unwrap_or(text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        match sender.dispatch(text, None).await {
            Ok(receipt) => {
                // Agent answers belong to the explicitly selected output route.
                // Only this line-mode caller receives them; stdout stays unused.
                let value = crate::audit::event(json!(receipt));
                eprintln!("{value}");
            }
            Err(error) => crate::client::render::status("chat input", "Error", &error),
        }
    }
    Ok(())
}

async fn serve_mcp(sender: Sender, lines: &mut Lines) -> Result<(), String> {
    let mut offered = false;
    let mut initialized = false;
    while let Some(line) = lines.next(FRAME_LIMIT, false).await? {
        let Ok(request) = serde_json::from_slice::<Value>(&line) else {
            write(rpc_error(Value::Null, -32700, "invalid JSON")).await?;
            continue;
        };
        let method = request["method"].as_str();
        if request["jsonrpc"] == "2.0"
            && method == Some("notifications/initialized")
            && request.get("id").is_none()
        {
            initialized = offered;
            continue;
        }
        if method == Some("tools/call") && request.get("id").is_some() && initialized {
            let id = request["id"].clone();
            let mut response = std::pin::pin!(rpc(&sender, request, initialized, None));
            loop {
                tokio::select! {
                    biased;
                    incoming = lines.next(FRAME_LIMIT, false) => {
                        let Some(incoming) = incoming? else { return Ok(()) };
                        if let Ok(other) = serde_json::from_slice::<Value>(&incoming)
                            && other["jsonrpc"] == "2.0"
                        {
                            if other["method"] == "notifications/cancelled"
                                && other.get("id").is_none() && other["params"]["requestId"] == id
                            {
                                // Dropping this future revokes its ticket. Already
                                // admitted native queue work is never replayed.
                                break;
                            }
                            if let Some(id) = other.get("id") {
                                write(rpc_error(id.clone(), -32000, "chat input is busy")).await?;
                            }
                        }
                    }
                    result = &mut response => {
                        if let Some(result) = result { write(result).await?; }
                        break;
                    }
                }
            }
            continue;
        }
        let initialize = method == Some("initialize");
        if let Some(result) = rpc(&sender, request, initialized, None).await {
            if initialize && result.get("result").is_some() {
                offered = true;
            }
            write(result).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_framing_preserves_split_utf8_and_rejects_overflow() {
        let mut lines = Lines {
            buffered: vec![0xe4, 0xbd],
        };
        assert!(lines.take_line(8).unwrap().is_none());
        lines
            .buffered
            .extend_from_slice(&[0xa0, b'\n', b'x', b'\n']);
        assert_eq!(lines.take_line(8).unwrap().unwrap(), "你\n".as_bytes());
        assert_eq!(lines.take_line(8).unwrap().unwrap(), b"x\n");
        lines.buffered.extend_from_slice(b"123456789");
        assert!(lines.take_line(8).is_err());
    }

    #[test]
    fn mcp_stdout_encoder_writes_only_one_json_line_even_with_multiline_output() {
        let value = json!({"jsonrpc":"2.0","id":7,"result":{"text":"first\nsecond"}});
        let wire = encode(&value).unwrap();
        assert_eq!(wire.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(serde_json::from_slice::<Value>(&wire).unwrap(), value);
    }
}
