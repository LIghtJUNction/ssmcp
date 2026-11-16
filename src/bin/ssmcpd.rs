use serde::Deserialize;
use ssmcp::{broker::Broker, http, identity::GitHubConfig, oauth::OAuthConfig};
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
const CONFIG_FILE: &str = "/etc/ssmcpd/ssmcpd.toml";
const HELP: &str = r#"ssmcpd - SSH-authenticated terminal MCP gateway

Usage:
  ssmcpd serve [--config PATH] [--public-url https://HOST] [--listen 127.0.0.1:8080] [--socket PATH]
  ssmcpd attach [--socket PATH]

Configuration:
  --config PATH selects a TOML file; SSMCP_CONFIG also selects a file.
  Otherwise /etc/ssmcpd/ssmcpd.toml is read when present.
  Precedence: command line > environment > TOML > built-in defaults.
  Portal: --portal-state-dir PATH overrides [portal] state_dir (private persistent directory).
  Server settings: [server] listen, public_url, socket.
  Environment: SSMCP_LISTEN, SSMCP_PUBLIC_URL, SSMCP_SOCKET.
  GitHub: [auth.github] client_id, client_secret (private file mode 0600).
  Callback: PUBLIC_URL/oauth/callback/github. Identity MCP endpoint: PUBLIC_URL/mcp.

Token lifetime options for serve (whole seconds; TOML keys are under [oauth]):
  --access-token-ttl N          SSMCP_ACCESS_TOKEN_TTL          [28800 / 8 hours]
  --refresh-token-idle-ttl N    SSMCP_REFRESH_TOKEN_IDLE_TTL    [2592000 / 30 days]
  --refresh-token-max-ttl N     SSMCP_REFRESH_TOKEN_MAX_TTL     [7776000 / 90 days]
  --client-registration-ttl N   SSMCP_CLIENT_REGISTRATION_TTL   [31536000 / 365 days]
  Required: 1 <= access <= refresh idle <= refresh max <= client <= 315360000.
  TOML keys use underscores, e.g. access_token_ttl = 28800.
  Login codes and authorization forms last 30 minutes.

Run serve in the foreground under systemd. OpenSSH runs attach as the authenticated user.
"#;

struct Config {
    listen: SocketAddr,
    public_url: String,
    socket: PathBuf,
    oauth: OAuthConfig,
    github: Option<GitHubConfig>,
    portal_dir: Option<PathBuf>,
}

// Plain TOML configuration. Never derive Debug or include source text in errors:
// the same file may contain an identity provider's client secret.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    server: ServerConfig,
    oauth: TokenConfig,
    auth: AuthConfig,
    portal: PortalConfig,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PortalConfig {
    state_dir: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ServerConfig {
    listen: Option<SocketAddr>,
    public_url: Option<String>,
    socket: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TokenConfig {
    access_token_ttl: Option<u64>,
    refresh_token_idle_ttl: Option<u64>,
    refresh_token_max_ttl: Option<u64>,
    client_registration_ttl: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AuthConfig {
    github: Option<GitHubConfig>,
}

fn parse_file_config(source: &str) -> ssmcp::Result<FileConfig> {
    toml::from_str(source).map_err(|error: toml::de::Error| {
        let line = error.span().map(|span| {
            source.as_bytes()[..span.start.min(source.len())]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count()
                + 1
        });
        format!(
            "invalid TOML configuration{}; check section names, value types, and quoting",
            line.map_or_else(String::new, |line| format!(" near line {line}"))
        )
        .into()
    })
}

fn load_file_config(path: &Path, config: &mut Config) -> ssmcp::Result<()> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("open configuration {}: {error}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err("configuration must be a regular file of at most 64 KiB".into());
    }
    let mut source = String::new();
    Read::by_ref(&mut file)
        .take(64 * 1024 + 1)
        .read_to_string(&mut source)?;
    if source.len() > 64 * 1024 {
        return Err("configuration exceeds 64 KiB".into());
    }
    let file = parse_file_config(&source)?;
    if file.auth.github.is_some() && metadata.mode() & 0o077 != 0 {
        return Err("configuration containing provider secrets must have mode 0600 and be owned by the service user".into());
    }
    if let Some(value) = file.server.listen {
        config.listen = value;
    }
    if let Some(value) = file.server.public_url {
        config.public_url = normalize_origin(&value)?;
    }
    if let Some(value) = file.server.socket {
        config.socket = value;
    }
    for (name, value, target) in [
        (
            "oauth.access_token_ttl",
            file.oauth.access_token_ttl,
            &mut config.oauth.access_token_ttl,
        ),
        (
            "oauth.refresh_token_idle_ttl",
            file.oauth.refresh_token_idle_ttl,
            &mut config.oauth.refresh_token_idle_ttl,
        ),
        (
            "oauth.refresh_token_max_ttl",
            file.oauth.refresh_token_max_ttl,
            &mut config.oauth.refresh_token_max_ttl,
        ),
        (
            "oauth.client_registration_ttl",
            file.oauth.client_registration_ttl,
            &mut config.oauth.client_registration_ttl,
        ),
    ] {
        if let Some(value) = value {
            *target = seconds(&value.to_string(), name)?;
        }
    }
    config.portal_dir = file.portal.state_dir;
    config.github = file.auth.github;
    Ok(())
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
    let remaining: Vec<String> = args.collect();
    if remaining
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print!("{HELP}");
        return Ok(());
    }
    let (pairs, remainder) = remaining.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(format!("missing value for {}", remaining.last().unwrap()).into());
    }
    let mut options = HashMap::new();
    for pair in pairs {
        if options.insert(pair[0].as_str(), pair[1].as_str()).is_some() {
            return Err(format!("duplicate option {}", pair[0]).into());
        }
    }
    let mut config = Config {
        listen: "127.0.0.1:8080".parse()?,
        public_url: String::new(),
        socket: SOCKET.into(),
        oauth: OAuthConfig::default(),
        github: None,
        portal_dir: None,
    };
    if mode == "serve" {
        let path = options
            .get("--config")
            .map(|path| PathBuf::from(*path))
            .or_else(|| std::env::var_os("SSMCP_CONFIG").map(PathBuf::from))
            .or_else(|| {
                Path::new(CONFIG_FILE)
                    .exists()
                    .then(|| PathBuf::from(CONFIG_FILE))
            });
        if let Some(path) = path {
            load_file_config(&path, &mut config)?;
        }
        // Environment retains compatibility with installations predating TOML.
        if !options.contains_key("--listen")
            && let Ok(value) = std::env::var("SSMCP_LISTEN")
        {
            config.listen = value.parse()?;
        }
        if !options.contains_key("--public-url")
            && let Ok(value) = std::env::var("SSMCP_PUBLIC_URL")
        {
            config.public_url = normalize_origin(&value)?;
        }
        if !options.contains_key("--socket")
            && let Some(value) = std::env::var_os("SSMCP_SOCKET")
        {
            config.socket = value.into();
        }
    }
    let mut lifetime_options = HashMap::new();
    for (arg, value) in options {
        match arg {
            "--config" if mode == "serve" => {}
            "--socket" => config.socket = value.into(),
            "--listen" if mode == "serve" => config.listen = value.parse()?,
            "--public-url" if mode == "serve" => config.public_url = normalize_origin(value)?,
            "--portal-state-dir" if mode == "serve" => config.portal_dir = Some(value.into()),
            "--access-token-ttl" if mode == "serve" => {
                lifetime_options.insert("SSMCP_ACCESS_TOKEN_TTL", seconds(value, arg)?);
            }
            "--refresh-token-idle-ttl" if mode == "serve" => {
                lifetime_options.insert("SSMCP_REFRESH_TOKEN_IDLE_TTL", seconds(value, arg)?);
            }
            "--refresh-token-max-ttl" if mode == "serve" => {
                lifetime_options.insert("SSMCP_REFRESH_TOKEN_MAX_TTL", seconds(value, arg)?);
            }
            "--client-registration-ttl" if mode == "serve" => {
                lifetime_options.insert("SSMCP_CLIENT_REGISTRATION_TTL", seconds(value, arg)?);
            }
            _ => return Err(format!("unknown option {arg}").into()),
        }
    }
    if mode == "attach" {
        return attach(&config.socket);
    }
    if config.public_url.is_empty() {
        return Err(
            "set server.public_url in TOML, SSMCP_PUBLIC_URL, or --public-url https://HOST".into(),
        );
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
    let mut broker = Broker::with_identity_config(config.public_url, config.oauth, config.github)?;
    if let Some(path) = config.portal_dir {
        if !broker.identities.enabled("github") {
            return Err("portal.state_dir requires the existing auth.github configuration".into());
        }
        broker.portal = ssmcp::portal::Portal::open(&path)?;
    }
    let broker = Arc::new(broker);
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
    fn toml_schema_rejects_typos_and_redacts_secret_source_in_errors() {
        let config = parse_file_config(
            r#"
[server]
public_url = "https://ssmcp.example"
[oauth]
access_token_ttl = 7200
[auth.github]
client_id = "example-client-id"
client_secret = "example-test-secret"
"#,
        )
        .unwrap();
        assert_eq!(config.oauth.access_token_ttl, Some(7200));
        assert_eq!(config.auth.github.unwrap().client_id, "example-client-id");
        for source in [
            "[auth.github]\nclient_id = 'id'\nclient_secret = 'marker-secret' extra",
            "[oauth]\naccess_token_ttl = 'marker-secret'",
            "[auth]\nclient_secret = 'marker-secret'",
            "[server]\npublic_urll = 'marker-secret'",
        ] {
            let error = parse_file_config(source).err().unwrap().to_string();
            assert!(error.contains("invalid TOML"));
            assert!(!error.contains("marker-secret"));
        }
    }

    #[test]
    fn toml_secrets_need_private_permissions_and_environment_overrides_file_values() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ssmcp-config-{}-{nonce}.toml", std::process::id()));
        std::fs::write(&path, "[server]\npublic_url = 'https://SSMCP.example:443/'\n[oauth]\naccess_token_ttl = 3600\n[auth.github]\nclient_id = 'test-id'\nclient_secret = 'test-secret'\n").unwrap();
        let mut config = Config {
            listen: "127.0.0.1:8080".parse().unwrap(),
            public_url: String::new(),
            socket: SOCKET.into(),
            oauth: OAuthConfig::default(),
            github: None,
            portal_dir: None,
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            load_file_config(&path, &mut config)
                .unwrap_err()
                .to_string()
                .contains("0600")
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        load_file_config(&path, &mut config).unwrap();
        assert_eq!(config.public_url, "https://ssmcp.example");
        assert_eq!(config.oauth.access_token_ttl.as_secs(), 3600);
        apply_oauth_options(&mut config.oauth, &HashMap::new(), |name| {
            (name == "SSMCP_ACCESS_TOKEN_TTL").then(|| "7200".into())
        })
        .unwrap();
        assert_eq!(config.oauth.access_token_ttl.as_secs(), 7200);
        std::fs::remove_file(path).unwrap();
    }

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
