//! HTTP over a private Unix socket, with the same router and OAuth as loopback.
use crate::local_socket;
use std::{
    ffi::OsString,
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt},
    },
    path::PathBuf,
    time::Duration,
};
use tokio::net::UnixStream;

// Linux sockaddr_un reserves one of its 108 bytes for the trailing NUL. The
// anchored bind path is shorter, but clients must also reach the advertised path.
const MAX_PATH_BYTES: usize = 107;

enum DefaultParent {
    Runtime,
    Temporary,
}

pub(super) struct Endpoint {
    pub path: PathBuf,
    default_parent: Option<DefaultParent>,
}

impl Endpoint {
    pub fn new(path: Option<PathBuf>) -> io::Result<Self> {
        Self::from_environment(
            path,
            std::env::var_os("XDG_RUNTIME_DIR"),
            super::effective_uid(),
        )
    }

    fn from_environment(
        path: Option<PathBuf>,
        runtime: Option<OsString>,
        uid: u32,
    ) -> io::Result<Self> {
        let (path, default_parent) = match path {
            Some(path) => (path, None),
            None => match runtime {
                Some(root) => (
                    PathBuf::from(root).join("ssmcp/mcp.sock"),
                    Some(DefaultParent::Runtime),
                ),
                None => (
                    PathBuf::from(format!("/tmp/ssmcp-{uid}/mcp.sock")),
                    Some(DefaultParent::Temporary),
                ),
            },
        };
        local_socket::syntax(&path)?;
        if path.as_os_str().as_bytes().contains(&0)
            || path.as_os_str().as_bytes().len() > MAX_PATH_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local MCP socket path must contain no NUL and fit within 107 bytes; use --socket with a shorter absolute path",
            ));
        }
        Ok(Self {
            path,
            default_parent,
        })
    }

    pub async fn bind(&self) -> io::Result<Listener> {
        if let Some(default_parent) = &self.default_parent {
            let parent = self.path.parent().expect("validated socket filename");
            let base = parent.parent().expect("validated default socket directory");
            match default_parent {
                DefaultParent::Runtime => {
                    // XDG_RUNTIME_DIR must already be a private, trusted directory.
                    // Reject an invalid value rather than selecting a different socket.
                    local_socket::SocketPath::open(&base.join("ssmcp-check.sock"))?;
                }
                DefaultParent::Temporary => {
                    let meta = fs::symlink_metadata(base)?;
                    if !meta.is_dir()
                        || meta.uid() != 0
                        || (meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "local MCP socket fallback requires a trusted root-owned /tmp",
                        ));
                    }
                }
            }
            match fs::DirBuilder::new().mode(0o700).create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let bound = local_socket::Bound::bind(&self.path)
            .await
            .map_err(|error| {
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::AddrInUse
                ) {
                    io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "local MCP socket is already active: {}",
                            self.path.display()
                        ),
                    )
                } else {
                    error
                }
            })?;
        Ok(Listener { bound })
    }
}

// Keep Bound, its writer lock and inode-aware cleanup alive for exactly as long
// as Axum owns this listener. A peer must match the effective UID before HTTP.
pub(super) struct Listener {
    bound: local_socket::Bound,
}

impl axum::serve::Listener for Listener {
    type Io = UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.bound.listener.accept().await {
                Ok((stream, address)) if local_socket::peer(&stream).is_ok() => {
                    return (stream, address);
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("local MCP socket accept failed: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.bound.listener.local_addr()
    }
}

#[cfg(test)]
mod tests;
