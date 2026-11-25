//! Bounded local control transport. The only discoverable address is loopback;
//! possession of the current user's private descriptor authenticates a caller.
use std::{
    fs::File,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

use crate::Result;

const FORMAT: u32 = 1;
const MAX_FRAME: usize = 256 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    format: u32,
    port: u16,
    token: String,
    pid: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    token: String,
    request: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    result: Option<Value>,
    error: Option<String>,
}

pub struct Server {
    listener: TcpListener,
    path: PathBuf,
    endpoint: Endpoint,
    _lock: File,
}

pub struct Peer {
    stream: TcpStream,
    token: String,
    authenticated: bool,
    replied: bool,
}

fn descriptor(directory: &Path, filename: &str) -> Result<PathBuf> {
    if filename.is_empty()
        || filename.len() > 64
        || !filename
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        || matches!(filename, "." | "..")
    {
        return Err("invalid local endpoint filename".into());
    }
    super::verify_directory(directory)?;
    Ok(directory.join(filename))
}

fn read_endpoint(path: &Path) -> Result<Endpoint> {
    let value: Endpoint = serde_json::from_slice(&super::read_private(path, 4096)?)
        .map_err(|_| "invalid local control descriptor")?;
    if value.format != FORMAT
        || value.port == 0
        || value.token.len() != 64
        || !value.token.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err("unsupported local control descriptor".into());
    }
    Ok(value)
}

fn token() -> Result<String> {
    let mut bytes = [0; 32];
    crate::platform::random_bytes(&mut bytes)?;
    Ok(crate::hex::encode(bytes))
}

impl Server {
    pub async fn bind(directory: &Path, filename: &str) -> Result<Self> {
        let path = descriptor(directory, filename)?;
        let guard = super::lock(&path.with_extension("endpoint-lock"), true)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = Endpoint {
            format: FORMAT,
            port: listener.local_addr()?.port(),
            token: token()?,
            pid: std::process::id(),
        };
        super::write_json(&path, &endpoint)?;
        Ok(Self {
            listener,
            path,
            endpoint,
            _lock: guard,
        })
    }

    /// Accept is deliberately separate from authentication so callers can bound
    /// concurrency and continue accepting while an unauthenticated peer stalls.
    pub async fn accept(&self) -> Result<Peer> {
        let (stream, address) = self.listener.accept().await?;
        if !address.ip().is_loopback() {
            return Err("local control requires loopback".into());
        }
        stream.set_nodelay(true)?;
        Ok(Peer {
            stream,
            token: self.endpoint.token.clone(),
            authenticated: false,
            replied: false,
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // A replacement runner may already have published its own descriptor.
        if read_endpoint(&self.path).is_ok_and(|value| {
            value.port == self.endpoint.port
                && bool::from(value.token.as_bytes().ct_eq(self.endpoint.token.as_bytes()))
        }) {
            let _ = super::remove_file(&self.path);
        }
    }
}

impl Peer {
    pub async fn read(&mut self) -> Result<Value> {
        if self.authenticated {
            return Err("local request was already read".into());
        }
        let bytes = timeout(IO_TIMEOUT, read_frame(&mut self.stream))
            .await
            .map_err(|_| "local request read timed out")??;
        let request: Request =
            serde_json::from_slice(&bytes).map_err(|_| "invalid local control request")?;
        if request.token.len() != self.token.len()
            || !bool::from(request.token.as_bytes().ct_eq(self.token.as_bytes()))
        {
            return Err("local control authentication failed".into());
        }
        self.authenticated = true;
        Ok(request.request)
    }

    pub async fn reply(&mut self, result: std::result::Result<Value, String>) -> Result<()> {
        if !self.authenticated || self.replied {
            return Err("local reply is not allowed".into());
        }
        self.replied = true;
        let reply = match result {
            Ok(value) => Reply {
                result: Some(value),
                error: None,
            },
            Err(error) => Reply {
                result: None,
                error: Some(error.chars().take(4096).collect()),
            },
        };
        timeout(
            IO_TIMEOUT,
            write_frame(&mut self.stream, &serde_json::to_vec(&reply)?),
        )
        .await
        .map_err(|_| "local reply write timed out")??;
        Ok(())
    }
}

pub async fn request(directory: &Path, filename: &str, request: Value) -> Result<Value> {
    request_with_timeout(directory, filename, request, REQUEST_TIMEOUT).await
}

pub async fn request_with_timeout(
    directory: &Path,
    filename: &str,
    request: Value,
    limit: Duration,
) -> Result<Value> {
    request_for_pid(directory, filename, request, limit, None).await
}

pub(super) async fn request_for_pid(
    directory: &Path,
    filename: &str,
    request: Value,
    limit: Duration,
    expected_pid: Option<u32>,
) -> Result<Value> {
    let endpoint = read_endpoint(&descriptor(directory, filename)?)?;
    if expected_pid.is_some_and(|pid| pid != endpoint.pid) {
        return Err("attach endpoint differs from the registered foreground process".into());
    }
    timeout(limit, async move {
        let mut stream = timeout(
            IO_TIMEOUT,
            TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port)),
        )
        .await
        .map_err(|_| "local control connection timed out")??;
        stream.set_nodelay(true)?;
        let payload = serde_json::to_vec(&Request {
            token: endpoint.token,
            request,
        })?;
        timeout(IO_TIMEOUT, write_frame(&mut stream, &payload))
            .await
            .map_err(|_| "local control write timed out")??;
        let reply: Reply = serde_json::from_slice(&read_frame(&mut stream).await?)
            .map_err(|_| "invalid local control reply")?;
        match (reply.result, reply.error) {
            (Some(value), None) => Ok(value),
            (None, Some(error)) => Err(error.into()),
            _ => Err("invalid local reply envelope".into()),
        }
    })
    .await
    .map_err(|_| "local operation timed out; inspect status before repeating a mutation")?
}

async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let size = stream.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err("local control frame exceeds its limit".into());
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_frame(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err("local control frame exceeds its limit".into());
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("ssmcp-ipc-{}", super::token().unwrap()));
            crate::platform::private_directory(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn authenticated_round_trip_and_descriptor_lifetime() {
        let directory = Directory::new();
        let server = Server::bind(&directory.0, "control.json").await.unwrap();
        assert!(Server::bind(&directory.0, "control.json").await.is_err());
        let client = request(&directory.0, "control.json", json!({"op":"status"}));
        let handler = async {
            let mut peer = server.accept().await.unwrap();
            assert_eq!(peer.read().await.unwrap(), json!({"op":"status"}));
            peer.reply(Ok(json!({"state":"running"}))).await.unwrap();
            assert!(peer.reply(Ok(Value::Null)).await.is_err());
        };
        let (result, _) = tokio::join!(client, handler);
        assert_eq!(result.unwrap()["state"], "running");
        drop(server);
        assert!(!directory.0.join("control.json").exists());
    }

    #[tokio::test]
    async fn rejects_wrong_token_and_oversized_frame_before_dispatch() {
        let directory = Directory::new();
        let server = Server::bind(&directory.0, "control.json").await.unwrap();
        let port = server.endpoint.port;
        let client = async {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            write_frame(
                &mut stream,
                &serde_json::to_vec(&Request {
                    token: "0".repeat(64),
                    request: json!({"op":"close"}),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        };
        let handler = async {
            let mut peer = server.accept().await.unwrap();
            assert!(
                peer.read()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("authentication")
            );
            assert!(peer.reply(Ok(Value::Null)).await.is_err());
        };
        tokio::join!(client, handler);
        let client = async {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            stream.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
        };
        let handler = async {
            let mut peer = server.accept().await.unwrap();
            assert!(peer.read().await.unwrap_err().to_string().contains("frame"));
        };
        tokio::join!(client, handler);
    }

    #[tokio::test]
    async fn cleanup_never_removes_replacement_descriptor() {
        let directory = Directory::new();
        let server = Server::bind(&directory.0, "control.json").await.unwrap();
        let replacement = Endpoint {
            token: token().unwrap(),
            format: FORMAT,
            port: server.endpoint.port,
            pid: 1,
        };
        super::super::write_json(&directory.0.join("control.json"), &replacement).unwrap();
        drop(server);
        assert_eq!(
            read_endpoint(&directory.0.join("control.json"))
                .unwrap()
                .token,
            replacement.token
        );
    }
}
