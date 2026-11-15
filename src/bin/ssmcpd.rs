use ssmcp::{broker::Broker, http, oauth::OAuthConfig};
use std::{
    collections::HashMap,
    ffi::OsString,
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpListener, UnixListener},
    sync::{Semaphore, oneshot},
    task::JoinSet,
};

const SOCKET: &str = "/run/ssmcpd/control.sock";
const HELP: &str = "ssmcpd - SSH-authenticated terminal MCP gateway\n\nUsage:\n  ssmcpd serve --public-url https://HOST [--listen 127.0.0.1:8080] [--socket PATH]\n  ssmcpd attach [--socket PATH]\n\nToken lifetime options for serve (whole seconds; command line overrides environment):\n  --access-token-ttl N          SSMCP_ACCESS_TOKEN_TTL          [28800 / 8 hours]\n  --refresh-token-idle-ttl N    SSMCP_REFRESH_TOKEN_IDLE_TTL    [2592000 / 30 days]\n  --refresh-token-max-ttl N     SSMCP_REFRESH_TOKEN_MAX_TTL     [7776000 / 90 days]\n  --client-registration-ttl N   SSMCP_CLIENT_REGISTRATION_TTL   [31536000 / 365 days]\n  Required: 1 <= access <= refresh idle <= refresh max <= client <= 315360000.\n  Login codes and authorization forms last 30 minutes.\n\nRun serve in the foreground under systemd. OpenSSH runs attach as the authenticated user.\n";

struct Config {
    listen: SocketAddr,
    public_url: String,
    socket: PathBuf,
    oauth: OAuthConfig,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ssmcpd: {error}");
        std::process::exit(1);
    }
}

fn run() -> ssmcp::Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "--help".into());
    if mode == "--help" || mode == "-h" {
        print!("{HELP}");
        return Ok(());
    }
    if mode == "--version" || mode == "-V" {
        println!("ssmcpd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if mode != "serve" && mode != "attach" {
        return Err(format!("unknown command {mode}; see --help").into());
    }
    let mut config = Config {
        listen: "127.0.0.1:8080".parse()?,
        public_url: String::new(),
        socket: SOCKET.into(),
        oauth: OAuthConfig::default(),
    };
    let mut lifetime_options = HashMap::new();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            print!("{HELP}");
            return Ok(());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--socket" => config.socket = value.into(),
            "--listen" if mode == "serve" => config.listen = value.parse()?,
            "--public-url" if mode == "serve" => config.public_url = normalize_origin(&value)?,
            "--access-token-ttl" if mode == "serve" => {
                lifetime_options.insert("SSMCP_ACCESS_TOKEN_TTL", seconds(&value, &arg)?);
            }
            "--refresh-token-idle-ttl" if mode == "serve" => {
                lifetime_options.insert("SSMCP_REFRESH_TOKEN_IDLE_TTL", seconds(&value, &arg)?);
            }
            "--refresh-token-max-ttl" if mode == "serve" => {
                lifetime_options.insert("SSMCP_REFRESH_TOKEN_MAX_TTL", seconds(&value, &arg)?);
            }
            "--client-registration-ttl" if mode == "serve" => {
                lifetime_options.insert("SSMCP_CLIENT_REGISTRATION_TTL", seconds(&value, &arg)?);
            }
            _ => return Err(format!("unknown option {arg}").into()),
        }
    }
    if mode == "attach" {
        return attach(&config.socket);
    }
    if config.public_url.is_empty() {
        return Err("--public-url https://HOST is required".into());
    }
    apply_oauth_options(&mut config.oauth, &lifetime_options, |name| {
        std::env::var_os(name)
    })?;
    config.oauth.validate()?;
    if !config.listen.ip().is_loopback() {
        return Err(
            "--listen must be a loopback address; expose HTTPS through a reverse proxy".into(),
        );
    }
    // SAFETY: this occurs before creating threads; private files are the daemon default.
    unsafe {
        libc::umask(0o077);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(config))
}

fn seconds(value: &str, name: &str) -> ssmcp::Result<Duration> {
    let value = value
        .parse::<u64>()
        .ok()
        .filter(|seconds| (1..=315_360_000).contains(seconds))
        .ok_or_else(|| format!("{name} must be an integer number of seconds in 1..=315360000"))?;
    Ok(Duration::from_secs(value))
}

fn apply_oauth_options(
    config: &mut OAuthConfig,
    overrides: &HashMap<&str, Duration>,
    mut environment: impl FnMut(&str) -> Option<OsString>,
) -> ssmcp::Result<()> {
    for (name, ttl) in [
        ("SSMCP_ACCESS_TOKEN_TTL", &mut config.access_token_ttl),
        (
            "SSMCP_REFRESH_TOKEN_IDLE_TTL",
            &mut config.refresh_token_idle_ttl,
        ),
        (
            "SSMCP_REFRESH_TOKEN_MAX_TTL",
            &mut config.refresh_token_max_ttl,
        ),
        (
            "SSMCP_CLIENT_REGISTRATION_TTL",
            &mut config.client_registration_ttl,
        ),
    ] {
        if let Some(value) = overrides.get(name) {
            *ttl = *value;
        } else if let Some(value) = environment(name) {
            *ttl = seconds(
                value
                    .to_str()
                    .ok_or_else(|| format!("{name} must be UTF-8"))?,
                name,
            )?;
        }
    }
    Ok(())
}

fn normalize_origin(input: &str) -> ssmcp::Result<String> {
    let uri: axum::http::Uri = input.parse()?;
    let authority = uri.authority().ok_or("--public-url needs a hostname")?;
    if uri.scheme_str() != Some("https")
        || authority.as_str().contains('@')
        || input.contains('#')
        || uri.query().is_some()
        || !matches!(uri.path(), "" | "/")
        || authority.host().is_empty()
    {
        return Err(
            "--public-url must be an HTTPS origin without credentials, path, query, or fragment"
                .into(),
        );
    }
    let port_suffix = &authority.as_str()[authority.host().len()..];
    if !port_suffix.is_empty()
        && (!port_suffix.starts_with(':') || authority.port_u16().is_none_or(|port| port == 0))
    {
        return Err("invalid public URL port".into());
    }
    let authority = authority.as_str().to_ascii_lowercase();
    Ok(format!(
        "https://{}",
        authority.strip_suffix(":443").unwrap_or(&authority)
    ))
}

fn attach(path: &Path) -> ssmcp::Result<()> {
    let mut output =
        UnixStream::connect(path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    let mut input = output.try_clone()?;
    // A pipe may remain blocked after the opposite direction closes. This helper has
    // no runtime or thread join on exit: returning from main closes all descriptors.
    std::thread::spawn(move || {
        let _ = copy_live(&mut io::stdin().lock(), &mut input);
        let _ = input.shutdown(Shutdown::Both);
    });
    let result = copy_live(&mut output, &mut io::stdout().lock());
    let _ = output.shutdown(Shutdown::Both);
    result?;
    Ok(())
}

fn copy_live(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<()> {
    let mut bytes = [0u8; 16 * 1024];
    loop {
        let count = match reader.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        // SSH stdio may itself be a socket. Avoid whole-stream kernel-copy
        // specializations: each protocol fragment must be visible immediately.
        writer.write_all(&bytes[..count])?;
        writer.flush()?;
    }
}

struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn bind_socket(path: &Path) -> ssmcp::Result<(UnixListener, SocketFile)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("socket needs an absolute parent directory")?;
    if !path.is_absolute() {
        return Err("--socket must be an absolute path".into());
    }
    if !parent.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750))?;
    }
    let metadata = std::fs::symlink_metadata(parent)?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(
            "socket directory must be owned by the daemon user and not group/world writable".into(),
        );
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() || meta.uid() != metadata.uid() {
                return Err("refusing to replace non-owned socket path".into());
            }
            match tokio::net::UnixStream::connect(path).await {
                Ok(_) => return Err("another ssmcpd is using this socket".into()),
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    let guard = SocketFile {
        path: path.into(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok((listener, guard))
}

async fn serve(config: Config) -> ssmcp::Result<()> {
    use std::future::IntoFuture;
    let tcp = TcpListener::bind(config.listen).await?;
    let (unix, _socket_file) = bind_socket(&config.socket).await?;
    let broker = Arc::new(Broker::with_oauth_config(config.public_url, config.oauth)?);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let server = axum::serve(tcp, http::router(broker.clone()))
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .into_future();
    tokio::pin!(server);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let connections = Arc::new(Semaphore::new(128));
    let mut tasks = JoinSet::new();
    eprintln!(
        "ssmcpd: listening on {}; control socket {}",
        config.listen,
        config.socket.display()
    );
    eprintln!(
        "ssmcpd: OAuth lifetimes (seconds): access={}, refresh idle={}, refresh max={}, client={}",
        config.oauth.access_token_ttl.as_secs(),
        config.oauth.refresh_token_idle_ttl.as_secs(),
        config.oauth.refresh_token_max_ttl.as_secs(),
        config.oauth.client_registration_ttl.as_secs()
    );
    let mut server_finished = false;
    let result = loop {
        tokio::select! {
            result = &mut server => { server_finished = true; break result.map_err(Into::into); }
            result = unix.accept() => {
                let (stream,_) = match result { Ok(pair) => pair, Err(e) => break Err(e.into()) };
                let Ok(permit) = connections.clone().try_acquire_owned() else { drop(stream); continue; };
                let broker = broker.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(e) = broker.attach(stream).await { eprintln!("ssmcpd: connection closed: {e}"); }
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(e) = result { eprintln!("ssmcpd: connection task failed: {e}"); }
            }
            result = &mut interrupt => { result?; break Ok(()); }
            _ = terminate.recv() => break Ok(()),
        }
    };
    broker.disconnect_all().await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let _ = stop_tx.send(());
    if !server_finished {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), &mut server).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_overrides_environment_without_parsing_shadowed_values() {
        let mut config = OAuthConfig::default();
        let overrides = HashMap::from([("SSMCP_ACCESS_TOKEN_TTL", Duration::from_secs(7200))]);
        apply_oauth_options(&mut config, &overrides, |name| match name {
            "SSMCP_ACCESS_TOKEN_TTL" => Some("invalid overridden value".into()),
            "SSMCP_REFRESH_TOKEN_IDLE_TTL" => Some("604800".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.access_token_ttl, Duration::from_secs(7200));
        assert_eq!(config.refresh_token_idle_ttl, Duration::from_secs(604800));
        assert_eq!(
            config.refresh_token_max_ttl,
            OAuthConfig::default().refresh_token_max_ttl
        );
        assert!(
            apply_oauth_options(&mut config, &HashMap::new(), |name| {
                (name == "SSMCP_ACCESS_TOKEN_TTL").then(|| "invalid".into())
            })
            .is_err()
        );
    }

    #[test]
    fn lifetime_options_require_bounded_positive_seconds() {
        assert_eq!(
            seconds("28800", "--access-token-ttl").unwrap().as_secs(),
            28800
        );
        for value in [
            "",
            "0",
            "-1",
            "1.5",
            "8h",
            "315360001",
            "18446744073709551616",
        ] {
            let message = seconds(value, "SSMCP_ACCESS_TOKEN_TTL")
                .unwrap_err()
                .to_string();
            assert!(message.contains("SSMCP_ACCESS_TOKEN_TTL"));
        }
    }
    #[test]
    fn public_url_is_an_https_origin() {
        assert_eq!(
            normalize_origin("https://SSMCP.example:443/").unwrap(),
            "https://ssmcp.example"
        );
        for bad in [
            "http://example",
            "https://user@example",
            "https://example/user",
            "https://example/?q=1",
            "https://example/#x",
            "https://example:999999",
        ] {
            assert!(normalize_origin(bad).is_err(), "{bad}");
        }
    }
}
