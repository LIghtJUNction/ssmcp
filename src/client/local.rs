//! Linux foreground MCP host using the same broker, wire and native runtime as
//! account connections. The Unix transport is private to this effective UID.
pub(super) mod install;
mod socket;

use crate::broker::Broker;
use std::{
    fs,
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, UnixListener, UnixStream},
    process::Command,
    sync::watch,
    task::{JoinHandle, JoinSet},
    time::timeout,
};

pub(super) const HELP: &str = r#"Usage: ssmcp local [options]

Run this Linux session without a remote server or GitHub login. Keep the
terminal open. The displayed MCP URL uses one-time connection-code OAuth.

  --listen IP:PORT       Loopback listener (default 127.0.0.1:0, automatic port)
  --socket [PATH]        Also serve HTTP/MCP on a private Unix socket
                         Default: $XDG_RUNTIME_DIR/ssmcp/mcp.sock, or
                         /tmp/ssmcp-<euid>/mcp.sock when XDG_RUNTIME_DIR is unset
                         Custom PATH: absolute, with an existing 0700 parent
  --public-url URL       HTTPS NAME.local origin of an existing local TLS proxy;
                         requires a fixed nonzero --listen port; excludes --socket
  --input MODE           Chat input (terminal by default; none disables chat)
  --agent RECIPIENT      Chat recipient (next-caller by default)
  --load-harness NAME    Preload one native harness; see ssmcp --list-harnesses
  --name NAME            Device name
  --title TEXT           Session title
  --context-root PATH    Additional local context directory
  --no-watchdog          Exit after a lost connection

Normal foreground display, shell and context flags also apply. Background
sessions and account connection options are unavailable in local mode.
Socket clients use the displayed HTTP origin and the same OAuth/token checks:
  curl --unix-socket PATH http://127.0.0.1:PORT/healthz
Replace PORT with the displayed port. The loopback listener remains available.
Only this OS user can connect; socket paths must fit within 107 bytes.

NAME.local requires working name resolution and a trusted TLS reverse proxy;
this command does not configure mDNS, certificates, hosts or firewall rules.
"#;

pub(super) struct Settings {
    pub listen: SocketAddr,
    pub public_url: Option<String>,
    socket: Option<socket::Endpoint>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            public_url: None,
            socket: None,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if self.socket.is_some() && self.public_url.is_some() {
            return Err("--socket cannot be combined with --public-url; Unix socket requests use the loopback HTTP origin".into());
        }
        if self.public_url.is_some() && self.listen.port() == 0 {
            return Err("--public-url requires --listen with a fixed nonzero loopback port for the existing TLS reverse proxy".into());
        }
        Ok(())
    }

    pub fn set_socket(&mut self, path: Option<PathBuf>) -> Result<(), String> {
        self.socket = Some(socket::Endpoint::new(path).map_err(|error| error.to_string())?);
        Ok(())
    }

    pub fn socket_path(&self) -> Option<&Path> {
        self.socket.as_ref().map(|endpoint| endpoint.path.as_path())
    }

    pub fn set_listen(&mut self, value: &str) -> Result<(), String> {
        let address: SocketAddr = value
            .parse()
            .map_err(|_| "--listen requires a literal loopback IP:PORT")?;
        if !address.ip().is_loopback() {
            return Err("local mode only binds literal loopback addresses".into());
        }
        self.listen = address;
        Ok(())
    }

    pub fn set_public_url(&mut self, value: &str) -> Result<(), String> {
        let origin = crate::oauth::local_origin(value).map_err(|error| error.to_string())?;
        if origin.scheme() != "https" {
            return Err(
                "--public-url requires https://NAME.local; use --listen for direct loopback HTTP"
                    .into(),
            );
        }
        self.public_url = Some(origin.origin().ascii_serialization());
        Ok(())
    }
}

struct SocketDirectory {
    path: PathBuf,
    dev: u64,
    ino: u64,
    runner: Option<(u64, u64)>,
}

impl SocketDirectory {
    fn create() -> crate::Result<Self> {
        // A fresh, private top-level directory avoids both cross-user defaults
        // and the pathname length limit of Unix sockets under long workspaces.
        let parent = Path::new("/tmp");
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.uid() != 0
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return Err("local socket parent /tmp has unsafe ownership or permissions".into());
        }
        let uid = effective_uid();
        for _ in 0..16 {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
            let suffix: String = crate::hex::encode(random);
            let path = parent.join(format!("ssmcp-local-{uid}-{suffix}"));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let metadata = fs::symlink_metadata(&path)?;
                    let directory = Self {
                        path,
                        dev: metadata.dev(),
                        ino: metadata.ino(),
                        runner: None,
                    };
                    validate_directory(&directory.path)?;
                    return Ok(directory);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not allocate a private local socket directory".into())
    }

    fn socket(&self) -> PathBuf {
        self.path.join("relay.sock")
    }

    fn install_runner(&mut self) -> io::Result<PathBuf> {
        let path = self.path.join("code-mode.js");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let metadata = file.metadata()?;
        // Record ownership before writing, so a failed startup also removes a
        // partially written file. Never replace a pre-existing runner path.
        self.runner = Some((metadata.dev(), metadata.ino()));
        file.write_all(include_bytes!("../code_mode.js"))?;
        Ok(path)
    }
}

impl Drop for SocketDirectory {
    fn drop(&mut self) {
        // Never recurse or remove a replaced directory, including after an
        // interrupted startup. Existing user files are not part of this host.
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if !metadata.is_dir()
            || metadata.dev() != self.dev
            || metadata.ino() != self.ino
            || metadata.uid() != effective_uid()
        {
            return;
        }
        let socket = self.socket();
        if fs::symlink_metadata(&socket).is_ok_and(|metadata| {
            metadata.file_type().is_socket() && metadata.uid() == effective_uid()
        }) {
            let _ = fs::remove_file(socket);
        }
        if let Some((dev, ino)) = self.runner {
            let path = self.path.join("code-mode.js");
            if fs::symlink_metadata(&path).is_ok_and(|metadata| {
                metadata.is_file()
                    && metadata.uid() == effective_uid()
                    && metadata.dev() == dev
                    && metadata.ino() == ino
            }) {
                let _ = fs::remove_file(path);
            }
        }
        let _ = fs::remove_dir(&self.path);
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no pointers and does not mutate process state.
    unsafe { libc::geteuid() }
}

fn validate_directory(path: &Path) -> crate::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() || metadata.mode() & 0o077 != 0 {
        return Err(
            "local socket directory must be private and owned by this effective user".into(),
        );
    }
    Ok(())
}

fn validate_socket(path: &Path) -> crate::Result<()> {
    validate_directory(path.parent().ok_or("local socket has no directory")?)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err("local socket must be private and owned by this effective user".into());
    }
    Ok(())
}

fn same_user(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local relay peer belongs to another effective user",
        ));
    }
    Ok(())
}

pub(super) struct Host {
    pub origin: String,
    pub socket: Option<PathBuf>,
    directory: SocketDirectory,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Host {
    pub async fn start(settings: &Settings, chat_disabled: bool) -> crate::Result<Self> {
        settings.validate()?;
        if !settings.listen.ip().is_loopback() {
            return Err("local mode only binds loopback addresses".into());
        }
        let socket_listener = match &settings.socket {
            Some(endpoint) => Some(endpoint.bind().await?),
            None => None,
        };
        let tcp = TcpListener::bind(settings.listen).await?;
        let origin = settings
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", tcp.local_addr().expect("bound listener")));
        let mut directory = SocketDirectory::create()?;
        let runner = directory.install_runner()?;
        let mut broker = Broker::local(origin.clone())?;
        broker.code_mode_runner = Some(runner);
        broker.local_chat_disabled = chat_disabled;
        let broker = Arc::new(broker);
        let unix = UnixListener::bind(directory.socket())?;
        fs::set_permissions(directory.socket(), fs::Permissions::from_mode(0o600))?;
        validate_socket(&directory.socket())?;
        let (stop, stopped) = watch::channel(false);
        let mut http_stop = stopped.clone();
        let router = crate::http::local_router(broker.clone());
        let socket_router = router.clone();
        let http = tokio::spawn(async move {
            let _ = axum::serve(tcp, router)
                .with_graceful_shutdown(async move {
                    let _ = http_stop.changed().await;
                })
                .await;
        });
        let relay = tokio::spawn(accept_local(unix, broker, stopped));
        let mut tasks = vec![relay, http];
        if let Some(listener) = socket_listener {
            let mut socket_stop = stop.subscribe();
            tasks.push(tokio::spawn(async move {
                let _ = axum::serve(listener, socket_router)
                    .with_graceful_shutdown(async move {
                        let _ = socket_stop.changed().await;
                    })
                    .await;
            }));
        }
        Ok(Self {
            origin,
            socket: settings.socket_path().map(Path::to_path_buf),
            directory,
            stop,
            tasks,
        })
    }

    pub fn command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .arg("__local-relay")
            .arg(self.directory.socket())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
            .env_remove("SSMCP_RELAY_SERVER")
            .env_remove("SSMCP_RELAY_TOKEN")
            .env_remove("SSMCP_RELAY_CAPABILITIES");
        command
    }

    pub async fn shutdown(mut self) {
        self.stop.send_replace(true);
        for mut task in self.tasks.drain(..) {
            if timeout(Duration::from_secs(8), &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn accept_local(
    listener: UnixListener,
    broker: Arc<Broker>,
    mut stop: watch::Receiver<bool>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            _ = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break; };
                if connections.len() < 16 && same_user(&stream).is_ok() {
                    let broker = broker.clone();
                    connections.spawn(async move { let _ = broker.attach(stream).await; });
                }
            }
        }
    }
    drop(listener);
    broker.disconnect_all().await;
    // An accepted peer may still be in its bounded Hello handshake. Dropping
    // the JoinSet cancels only connections owned by this local host.
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

pub(super) async fn relay() -> crate::Result<()> {
    let mut args = std::env::args_os().skip(2);
    let path = PathBuf::from(
        args.next()
            .ok_or("local relay requires its private socket")?,
    );
    if args.next().is_some() || !path.is_absolute() {
        return Err("local relay requires one absolute socket path".into());
    }
    validate_socket(&path)?;
    let stream = UnixStream::connect(path).await?;
    same_user(&stream)?;
    let (mut read, mut write) = stream.into_split();
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    tokio::select! {
        result = tokio::io::copy(&mut input, &mut write) => { result?; write.shutdown().await?; },
        result = tokio::io::copy(&mut read, &mut output) => { result?; output.flush().await?; },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_runner_is_private_and_removed_after_failed_startup() {
        fn fail_after_runner(path: &mut PathBuf) -> crate::Result<()> {
            let mut directory = SocketDirectory::create()?;
            *path = directory.path.clone();
            let runner = directory.install_runner()?;
            let metadata = fs::symlink_metadata(&runner)?;
            assert!(metadata.is_file());
            assert_eq!(metadata.uid(), effective_uid());
            assert_eq!(metadata.mode() & 0o777, 0o600);
            assert_eq!(fs::read(runner)?, include_bytes!("../code_mode.js"));
            Err("injected failure after local runner creation".into())
        }

        let mut path = PathBuf::new();
        assert!(fail_after_runner(&mut path).is_err());
        assert!(!path.as_os_str().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn bundled_runner_never_overwrites_or_removes_a_replaced_file() {
        let mut directory = SocketDirectory::create().unwrap();
        let path = directory.path.join("code-mode.js");
        fs::write(&path, b"pre-existing fixture").unwrap();
        assert_eq!(
            directory.install_runner().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let root = directory.path.clone();
        drop(directory);
        assert_eq!(fs::read(&path).unwrap(), b"pre-existing fixture");
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();

        let mut directory = SocketDirectory::create().unwrap();
        let path = directory.install_runner().unwrap();
        let replacement = directory.path.join("replacement");
        fs::write(&replacement, b"replacement fixture").unwrap();
        fs::rename(replacement, &path).unwrap();
        let root = directory.path.clone();
        drop(directory);
        assert_eq!(fs::read(&path).unwrap(), b"replacement fixture");
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn settings_require_loopback_and_explicit_https_local_proxy() {
        let mut settings = Settings::default();
        assert!(settings.set_listen("0.0.0.0:8080").is_err());
        assert!(settings.set_listen("192.0.2.1:8080").is_err());
        assert!(settings.set_listen("localhost:8080").is_err());
        settings.set_listen("[::1]:0").unwrap();
        assert!(settings.set_public_url("http://127.0.0.1:8080").is_err());
        assert!(settings.set_public_url("https://remote.example").is_err());
        settings
            .set_public_url("https://workstation.local:8443/")
            .unwrap();
        assert_eq!(
            settings.public_url.as_deref(),
            Some("https://workstation.local:8443")
        );
        assert!(settings.validate().is_err());
        settings.set_listen("127.0.0.1:8765").unwrap();
        settings.validate().unwrap();
        settings
            .set_socket(Some("/tmp/private/mcp.sock".into()))
            .unwrap();
        assert!(
            settings
                .validate()
                .unwrap_err()
                .contains("cannot be combined")
        );
    }

    #[tokio::test]
    async fn private_socket_rejects_symlinks_and_exposed_permissions() {
        use std::os::unix::fs::symlink;
        let directory = SocketDirectory::create().unwrap();
        assert_eq!(
            fs::symlink_metadata(&directory.path).unwrap().mode() & 0o777,
            0o700
        );
        let listener = UnixListener::bind(directory.socket()).unwrap();
        fs::set_permissions(directory.socket(), fs::Permissions::from_mode(0o600)).unwrap();
        validate_socket(&directory.socket()).unwrap();
        let link = directory.path.join("alias.sock");
        symlink(directory.socket(), &link).unwrap();
        assert!(validate_socket(&link).is_err());
        fs::remove_file(link).unwrap();
        fs::set_permissions(&directory.path, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(validate_socket(&directory.socket()).is_err());
        fs::set_permissions(&directory.path, fs::Permissions::from_mode(0o700)).unwrap();
        let (left, right) = UnixStream::pair().unwrap();
        same_user(&left).unwrap();
        same_user(&right).unwrap();
        drop(listener);
    }

    #[tokio::test]
    async fn gateway_checks_all_origins_and_has_no_portal_routes() {
        let host = Host::start(&Settings::default(), false).await.unwrap();
        let client = crate::http_client::builder().no_proxy().build().unwrap();
        let url = format!("{}/healthz", host.origin);
        assert_eq!(client.get(&url).send().await.unwrap().status(), 200);
        assert_eq!(
            client
                .get(&url)
                .header("Host", "attacker.invalid")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .get(&url)
                .header("Origin", "https://attacker.invalid")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .get(&url)
                .header("Origin", &host.origin)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            client
                .get(format!("{}/oauth/authorize", host.origin))
                .header("Origin", "null")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        for path in [
            "/account/login",
            "/api/client/connect",
            "/api/device/logout",
        ] {
            assert_eq!(
                client
                    .get(format!("{}{path}", host.origin))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                404
            );
        }
        let socket = host.directory.socket();
        let directory = host.directory.path.clone();
        host.shutdown().await;
        assert!(!socket.exists());
        assert!(!directory.exists());
        assert!(client.get(url).send().await.is_err());
    }
}
