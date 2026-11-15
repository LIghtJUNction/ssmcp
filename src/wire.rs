//! Private, bounded, newline-delimited JSON protocol carried inside SSH.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u32 = 4;
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
pub const MAX_READ: usize = 32 * 1024;
pub const MAX_WRITE: usize = 16 * 1024;
pub const MAX_WAIT_MS: u64 = 5_000;
pub const MAX_INFLIGHT: usize = 16;
pub const MAX_CLIENTS_PER_USER: usize = 128;

/// Display metadata supplied by the local client, never an authentication identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientMetadata {
    pub name: String,
    /// Absolute working directory at client startup; agent sessions may use other directories.
    pub cwd: String,
    pub hostname: String,
}

impl ClientMetadata {
    pub fn validate(&self) -> Result<(), String> {
        for (label, value, limit) in [
            ("name", self.name.as_str(), 128),
            ("cwd", self.cwd.as_str(), 4096),
            ("hostname", self.hostname.as_str(), 255),
        ] {
            if value.trim().is_empty() || value.len() > limit || value.chars().any(char::is_control)
            {
                return Err(format!(
                    "client {label} must be nonempty, contain no control characters, and be at most {limit} bytes"
                ));
            }
        }
        if !std::path::Path::new(&self.cwd).is_absolute() {
            return Err("client cwd must be an absolute path".into());
        }
        Ok(())
    }
}

/// Public discovery information. IDs identify a connection, not a display name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientInfo {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub mode: Mode,
    pub cwd: String,
    pub hostname: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Shell,
    Codex,
    Cc,
    Pi,
    Opencode,
}

impl Mode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "shell" => Ok(Self::Shell),
            "codex" => Ok(Self::Codex),
            "cc" => Ok(Self::Cc),
            "pi" => Ok(Self::Pi),
            "opencode" => Ok(Self::Opencode),
            _ => Err("-t must be shell, codex, cc, pi, or opencode".into()),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Codex => "codex",
            Self::Cc => "cc",
            Self::Pi => "pi",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Read {
        #[serde(default)]
        cursor: u64,
        #[serde(default = "default_max_bytes")]
        max_bytes: usize,
        #[serde(default)]
        wait_ms: u64,
    },
    Write {
        data: String,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Status {},
    ListTools {
        #[serde(default)]
        cursor: Option<String>,
    },
    CallTool {
        name: String,
        arguments: Value,
    },
}

fn default_max_bytes() -> usize {
    16 * 1024
}

impl Operation {
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self {
            Self::Read {
                max_bytes, wait_ms, ..
            } if *max_bytes == 0 || *max_bytes > MAX_READ || *wait_ms > MAX_WAIT_MS => Err(
                format!("max_bytes must be 1..={MAX_READ}; wait_ms must be 0..={MAX_WAIT_MS}"),
            ),
            Self::Write { data } if data.len() > MAX_WRITE => {
                Err(format!("data exceeds {MAX_WRITE} bytes"))
            }
            Self::Resize { rows, cols }
                if *rows == 0 || *cols == 0 || *rows > 1000 || *cols > 1000 =>
            {
                Err("rows and cols must be 1..=1000".into())
            }
            Self::ListTools {
                cursor: Some(cursor),
            } if cursor.len() > 128 => Err("tool cursor is too long".into()),
            Self::CallTool { name, arguments }
                if name.is_empty()
                    || name.len() > 128
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                    || !arguments.is_object() =>
            {
                Err("invalid tool name or arguments".into())
            }
            _ => Ok(()),
        }
    }
}

// Do not derive Debug: Ready carries a bearer secret.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Hello {
        version: u32,
        mode: Mode,
        metadata: ClientMetadata,
    },
    Ready {
        url: String,
        secret: String,
        client_id: String,
    },
    Call {
        id: u64,
        operation: Operation,
    },
    Reply {
        id: u64,
        result: std::result::Result<Value, String>,
    },
    Reject {
        message: String,
    },
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut line = Vec::new();
    loop {
        let data = reader.fill_buf().await?;
        if data.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete frame",
                ))
            };
        }
        let n = data
            .iter()
            .position(|b| *b == b'\n')
            .map_or(data.len(), |i| i + 1);
        if line.len() + n > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds limit",
            ));
        }
        let complete = data[n - 1] == b'\n';
        line.extend_from_slice(&data[..n]);
        reader.consume(n);
        if complete {
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds limit",
        ));
    }
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounded_frames_reject_truncation_and_oversize() {
        let mut truncated = &b"{\"type\":\"hello\",\"version\":1}"[..];
        assert_eq!(
            read_frame(&mut truncated).await.err().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let huge = vec![b'x'; MAX_FRAME + 1];
        assert_eq!(
            read_frame(&mut huge.as_slice()).await.err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(read_frame(&mut &b""[..]).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn frames_round_trip() {
        let mut out = Vec::new();
        write_frame(
            &mut out,
            &Frame::Hello {
                version: VERSION,
                mode: Mode::Shell,
                metadata: ClientMetadata {
                    name: "dev".into(),
                    cwd: "/project".into(),
                    hostname: "localhost".into(),
                },
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame(&mut out.as_slice()).await.unwrap(),
            Some(Frame::Hello {
                version: VERSION,
                mode: Mode::Shell,
                metadata: ClientMetadata { name, cwd, hostname },
            }) if name == "dev" && cwd == "/project" && hostname == "localhost"
        ));
    }
    #[test]
    fn client_metadata_rejects_relative_paths_controls_and_unbounded_fields() {
        let valid = ClientMetadata {
            name: "My workstation".into(),
            cwd: "/home/user/project".into(),
            hostname: "workstation".into(),
        };
        assert!(valid.validate().is_ok());
        for name in ["".into(), "  ".into(), "bad\nname".into(), "x".repeat(129)] {
            assert!(
                ClientMetadata {
                    name,
                    ..valid.clone()
                }
                .validate()
                .is_err()
            );
        }
        for cwd in [
            "relative".into(),
            "/bad\0path".into(),
            format!("/{}", "x".repeat(4096)),
        ] {
            assert!(
                ClientMetadata {
                    cwd,
                    ..valid.clone()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            ClientMetadata {
                hostname: "x".repeat(256),
                ..valid
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn rejects_unknown_fields_and_invalid_limits() {
        assert!(
            serde_json::from_value::<Operation>(
                serde_json::json!({"op":"status","command":"ignored"})
            )
            .is_err()
        );
        assert!(
            Operation::Read {
                cursor: 0,
                max_bytes: MAX_READ + 1,
                wait_ms: 0
            }
            .validate()
            .is_err()
        );
        assert!(Operation::Resize { rows: 0, cols: 80 }.validate().is_err());
    }
}
