//! Replaceable renderer for public guide pages and known static assets only.
//! The core never forwards cookies, bearer tokens, account state, or headers.
use crate::local_socket as socket;
#[cfg(test)]
mod tests;

use super::{assets, guide};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};

const PROTOCOL: u32 = 1;
const MAX_REQUEST: usize = 4096;
const MAX_HEADER: usize = 1024;
const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_CONCURRENT: usize = 16;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(250);
const WORKER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    Guide {
        language: guide::Language,
        public_url: String,
    },
    GuideCss {},
    Asset {
        name: String,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    protocol: u32,
    content_type: String,
    sha256: String,
}
pub(super) struct Rendered {
    pub(super) body: Vec<u8>,
    pub(super) fingerprint: String,
}
struct Page {
    content_type: &'static str,
    body: Cow<'static, [u8]>,
}
impl Page {
    fn resolve(request: Request) -> io::Result<Self> {
        match request {
            Request::Guide {
                language,
                public_url,
            } => Ok(Self {
                content_type: "text/html; charset=utf-8",
                body: guide::page(language, &public_url).into_bytes().into(),
            }),
            Request::GuideCss {} => Ok(Self {
                content_type: "text/css; charset=utf-8",
                body: Cow::Borrowed(guide::CSS.as_bytes()),
            }),
            Request::Asset { name } => {
                let (content_type, body) = assets::public_bytes(&name).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "unknown public asset")
                })?;
                Ok(Self {
                    content_type,
                    body: Cow::Borrowed(body),
                })
            }
        }
    }
}

pub(super) struct Endpoint {
    path: PathBuf,
    slots: Arc<Semaphore>,
}
impl Endpoint {
    pub(super) fn new(path: PathBuf) -> io::Result<Self> {
        socket::syntax(&path)?;
        // A worker may be started later. Existing paths must already be safe;
        // every request validates the complete path again before connecting.
        match socket::SocketPath::open(&path) {
            Ok(socket) => match socket.verify() {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(Self {
            path,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        })
    }
    pub(super) async fn render(
        &self,
        request: Request,
        content_type: &str,
        expected_hash: Option<&str>,
    ) -> Option<Rendered> {
        let _permit = self.slots.try_acquire().ok()?;
        timeout(REQUEST_TIMEOUT, async {
            let path = socket::SocketPath::open(&self.path)?;
            path.verify()?;
            let mut stream = UnixStream::connect(path.anchored()).await?;
            socket::peer(&stream)?;
            let bytes = serde_json::to_vec(&request).map_err(io::Error::other)?;
            write_frame(&mut stream, &bytes, MAX_REQUEST).await?;
            let header: Header = serde_json::from_slice(
                &read_frame(&mut stream, MAX_HEADER).await?,
            )
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid public frontend header")
            })?;
            if header.protocol != PROTOCOL || header.content_type != content_type {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "public frontend protocol mismatch",
                ));
            }
            let body = read_frame(&mut stream, MAX_BODY).await?;
            let fingerprint = crate::hex::encode(Sha256::digest(&body));
            if fingerprint != header.sha256 || expected_hash.is_some_and(|hash| hash != fingerprint)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "public asset fingerprint mismatch",
                ));
            }
            Ok(Rendered { body, fingerprint })
        })
        .await
        .ok()?
        .ok()
    }
}

async fn read_frame(stream: &mut UnixStream, maximum: usize) -> io::Result<Vec<u8>> {
    let size = stream.read_u32().await? as usize;
    if size > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frontend frame exceeds limit",
        ));
    }
    let mut body = vec![0; size];
    stream.read_exact(&mut body).await?;
    Ok(body)
}
async fn write_frame(stream: &mut UnixStream, bytes: &[u8], maximum: usize) -> io::Result<()> {
    if bytes.len() > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frontend frame exceeds limit",
        ));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await
}
async fn serve_one(mut stream: UnixStream) -> io::Result<()> {
    socket::peer(&stream)?;
    let request: Request = serde_json::from_slice(&read_frame(&mut stream, MAX_REQUEST).await?)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid public frontend request",
            )
        })?;
    let page = Page::resolve(request)?;
    let header = Header {
        protocol: PROTOCOL,
        content_type: page.content_type.into(),
        sha256: crate::hex::encode(Sha256::digest(&page.body)),
    };
    write_frame(
        &mut stream,
        &serde_json::to_vec(&header).map_err(io::Error::other)?,
        MAX_HEADER,
    )
    .await?;
    write_frame(&mut stream, &page.body, MAX_BODY).await?;
    stream.shutdown().await
}
async fn run(bound: socket::Bound) -> io::Result<()> {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            incoming = bound.listener.accept() => {
                let (stream, _) = incoming?;
                if tasks.len() >= MAX_CONCURRENT || socket::peer(&stream).is_err() { continue; }
                tasks.spawn(async move { let _ = timeout(WORKER_TIMEOUT, serve_one(stream)).await; });
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

/// Serve only public rendering requests. Signal shutdown drops the listener,
/// cancels owned handlers and removes only this process's socket inode.
pub async fn serve(path: &Path) -> io::Result<()> {
    let bound = socket::Bound::bind(path).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = run(bound) => result,
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}
