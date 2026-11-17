use serde::Deserialize;
use ssmcp::{
    broker::Broker,
    history::{AuditConfig, AuditLog},
    http,
    identity::GitHubConfig,
    management::{ManagementPolicy, ManagementServer},
    oauth::OAuthConfig,
};
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
  ssmcpd serve [--config PATH] [--public-url https://HOST] [--listen 127.0.0.1:8080] [--socket PATH] [--portal-only]
  ssmcpd attach [--socket PATH]

Configuration:
  --config PATH selects a TOML file; SSMCP_CONFIG also selects a file.
  Otherwise /etc/ssmcpd/ssmcpd.toml is read when present.
  Precedence: command line > environment > TOML > built-in defaults.
  Portal: --portal-state-dir PATH overrides [portal] state_dir (private persistent directory).
  --portal-only runs the account HTTP portal without binding an SSH attachment socket.
  --gateway-socket PATH overrides [portal] gateway_socket (private gateway management socket).
  Management: [portal] admin_github_ids, admin_github_users, owner_mappings.
  Admin IDs are immutable GitHub numeric ID strings; username entries resolve once at startup.
  Audit: [portal] audit_dir, audit_retention_days (default 7), audit_max_events (default 20000).
  The gateway requires audit_dir when gateway_socket is configured. Portal-only is a socket client.
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
  --client-registry PATH       SSMCP_CLIENT_REGISTRY
  TOML: [oauth] client_registry = "/var/lib/ssmcpd/oauth-clients.toml".
  Installed systemd service defaults to its StateDirectory automatically.
  Parent directory must exist, be daemon-owned and mode 0700; files use 0600.
  Registrations survive restart; tokens and unfinished logins still require reauthorization.
  Login codes and authorization forms last 30 minutes.

Run serve in the foreground under systemd. OpenSSH runs attach as the authenticated user.
"#;

struct Config {
    listen: SocketAddr,
    public_url: String,
    socket: PathBuf,
    oauth: OAuthConfig,
    github: Option<GitHubConfig>,
    client_registry: Option<PathBuf>,
    portal: PortalConfig,
    portal_only: bool,
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

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PortalConfig {
    state_dir: Option<PathBuf>,
    gateway_socket: Option<PathBuf>,
    admin_github_ids: Vec<String>,
    admin_github_users: Vec<String>,
    owner_mappings: HashMap<String, String>,
    audit_dir: Option<PathBuf>,
    audit_retention_days: u32,
    audit_max_events: usize,
}

impl Default for PortalConfig {
    fn default() -> Self {
        let audit = AuditConfig::default();
        Self {
            state_dir: None,
            gateway_socket: None,
            admin_github_ids: Vec::new(),
            admin_github_users: Vec::new(),
            owner_mappings: HashMap::new(),
            audit_dir: None,
            audit_retention_days: audit.retention_days,
            audit_max_events: audit.max_events,
        }
    }
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
    client_registry: Option<PathBuf>,
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
    // Authorization policy is security-sensitive even when the file contains no provider secret.
    // SAFETY: geteuid has no preconditions.
    if metadata.mode() & 0o022 != 0 || ![0, unsafe { libc::geteuid() }].contains(&metadata.uid()) {
        return Err("configuration must be root/service-owned and not group/world writable".into());
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
    config.client_registry = file.oauth.client_registry;
    config.portal = file.portal;
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
    let (options, portal_only) = parse_options(&remaining, &mode)?;
    let mut config = Config {
        listen: "127.0.0.1:8080".parse()?,
        public_url: String::new(),
        socket: SOCKET.into(),
        oauth: OAuthConfig::default(),
        github: None,
        client_registry: None,
        portal: PortalConfig::default(),
        portal_only,
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
        if !options.contains_key("--client-registry")
            && let Some(value) = std::env::var_os("SSMCP_CLIENT_REGISTRY")
        {
            config.client_registry = Some(value.into());
        }
        if config.client_registry.is_none() {
            config.client_registry = std::env::var_os("STATE_DIRECTORY")
                .map(|path| PathBuf::from(path).join("oauth-clients.toml"));
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
            "--client-registry" if mode == "serve" => config.client_registry = Some(value.into()),
            "--socket" => config.socket = value.into(),
            "--listen" if mode == "serve" => config.listen = value.parse()?,
            "--public-url" if mode == "serve" => config.public_url = normalize_origin(value)?,
            "--portal-state-dir" if mode == "serve" => config.portal.state_dir = Some(value.into()),
            "--gateway-socket" if mode == "serve" => {
                config.portal.gateway_socket = Some(value.into())
            }
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
    validate_portal_config(&config)?;
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

fn parse_options<'a>(
    args: &'a [String],
    mode: &str,
) -> ssmcp::Result<(HashMap<&'a str, &'a str>, bool)> {
    let mut options = HashMap::new();
    let mut portal_only = false;
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--portal-only" {
            if mode != "serve" {
                return Err("--portal-only is available only with serve".into());
            }
            if portal_only {
                return Err("duplicate option --portal-only".into());
            }
            portal_only = true;
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("missing value for {option}"))?;
        if options.insert(option, value.as_str()).is_some() {
            return Err(format!("duplicate option {option}").into());
        }
        index += 2;
    }
    Ok((options, portal_only))
}

fn absolute_path(path: &Path, label: &str) -> ssmcp::Result<()> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!("{label} must be an absolute path without '..'").into());
    }
    Ok(())
}

fn validate_portal_config(config: &Config) -> ssmcp::Result<()> {
    let portal = &config.portal;
    AuditConfig {
        retention_days: portal.audit_retention_days,
        max_events: portal.audit_max_events,
    }
    .validate()?;
    ManagementPolicy::new(
        portal.admin_github_ids.clone(),
        portal.owner_mappings.clone(),
    )?;
    for login in &portal.admin_github_users {
        ssmcp::wire::canonical_github_principal(&format!("{login}@github"))
            .map_err(|_| "portal.admin_github_users contains an invalid GitHub username")?;
    }
    if config.portal_only && portal.state_dir.is_none() {
        return Err("--portal-only requires portal.state_dir or --portal-state-dir".into());
    }
    match &portal.gateway_socket {
        Some(path) => {
            absolute_path(path, "portal.gateway_socket")?;
            if path == &config.socket {
                return Err(
                    "portal.gateway_socket must differ from the SSH attachment socket".into(),
                );
            }
            if !config.portal_only && portal.audit_dir.is_none() {
                return Err(
                    "gateway management requires portal.audit_dir for durable audit history".into(),
                );
            }
        }
        None => {
            if !portal.admin_github_ids.is_empty()
                || !portal.admin_github_users.is_empty()
                || !portal.owner_mappings.is_empty()
                || portal.audit_dir.is_some()
            {
                return Err("portal administrator, ownership and audit configuration requires gateway_socket".into());
            }
        }
    }
    if let Some(path) = &portal.audit_dir {
        absolute_path(path, "portal.audit_dir")?;
    }
    Ok(())
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
    let mut broker = Broker::with_identity_config(config.public_url, config.oauth, config.github)?;
    if let Some(path) = config.client_registry {
        broker.oauth.enable_client_registry(&path)?;
        ssmcp::observability::log(
            "info",
            "persistence",
            &[("event", "durable_client_registry_enabled".into())],
        );
    } else {
        ssmcp::observability::log(
            "warn",
            "persistence",
            &[("event", "client_registry_memory_only".into())],
        );
    }
    if let Some(path) = &config.portal.state_dir {
        if !broker.identities.enabled("github") {
            return Err("portal.state_dir requires the existing auth.github configuration".into());
        }
        broker.portal = ssmcp::portal::Portal::open(path)?;
        if let Some(socket) = &config.portal.gateway_socket {
            broker.portal.configure_management(socket.clone());
        }
    }

    // A portal-only process never opens audit storage or binds either gateway socket.
    let mut audit = None;
    let management_server = if !config.portal_only
        && let Some(socket) = &config.portal.gateway_socket
    {
        let mut admins = config.portal.admin_github_ids;
        if !config.portal.admin_github_users.is_empty() {
            let principals = config
                .portal
                .admin_github_users
                .iter()
                .map(|login| format!("{login}@github"))
                .collect::<Vec<_>>();
            let resolved = broker.identities.resolve_allow(&principals).await.map_err(
                |_| "could not resolve portal administrator GitHub usernames; refusing to start",
            )?;
            admins.extend(resolved.into_iter().map(|identity| identity.subject));
        }
        let policy = ManagementPolicy::new(admins, config.portal.owner_mappings)?;
        let log = Arc::new(AuditLog::open(
            config
                .portal
                .audit_dir
                .as_deref()
                .ok_or("gateway management requires portal.audit_dir")?,
            AuditConfig {
                retention_days: config.portal.audit_retention_days,
                max_events: config.portal.audit_max_events,
            },
        )?);
        broker.configure_management(policy, log.clone());
        audit = Some(log);
        Some(ManagementServer::bind(socket).await?)
    } else {
        None
    };
    let (unix, _socket_file) = if config.portal_only {
        (None, None)
    } else {
        let (listener, guard) = bind_socket(&config.socket).await?;
        (Some(listener), Some(guard))
    };
    let tcp = TcpListener::bind(config.listen).await?;
    let broker = Arc::new(broker);
    let management_broker = broker.clone();
    let management_loop = async move {
        match management_server {
            Some(server) => server.run(management_broker).await,
            None => std::future::pending::<io::Result<()>>().await,
        }
    };
    tokio::pin!(management_loop);
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
    let mut prune = tokio::time::interval(Duration::from_secs(60));
    prune.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ssmcp::observability::log(
        "info",
        "session",
        &[
            ("event", "listening".into()),
            ("listen", config.listen.to_string()),
            (
                "mode",
                if config.portal_only {
                    "portal_only"
                } else {
                    "gateway"
                }
                .into(),
            ),
        ],
    );
    let mut server_finished = false;
    let result = loop {
        tokio::select! {
            result = &mut server => { server_finished = true; break result.map_err(Into::into); }
            result = &mut management_loop => { break result.map_err(Into::into); }
            result = async {
                match &unix {
                    Some(listener) => listener.accept().await,
                    None => std::future::pending().await,
                }
            } => {
                let (stream,_) = match result { Ok(pair) => pair, Err(e) => break Err(e.into()) };
                let Ok(permit) = connections.clone().try_acquire_owned() else { drop(stream); continue; };
                let broker = broker.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if broker.attach(stream).await.is_err() { ssmcp::observability::log("warn","session",&[("event","attach_failed".into())]); }
                });
            }
            _ = prune.tick(), if audit.is_some() => {
                let log = audit.as_ref().expect("guarded audit timer").clone();
                if !matches!(tokio::task::spawn_blocking(move || log.prune()).await, Ok(Ok(_))) {
                    ssmcp::observability::log("error", "persistence", &[("event", "audit_prune_failed".into())]);
                }
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if result.is_err() { ssmcp::observability::log("error","session",&[("event","connection_task_failed".into())]); }
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
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut server).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portal_config_fixture() -> Config {
        Config {
            listen: "127.0.0.1:8080".parse().unwrap(),
            public_url: "https://ssmcp.test".into(),
            socket: "/tmp/control.sock".into(),
            oauth: OAuthConfig::default(),
            github: None,
            client_registry: None,
            portal: PortalConfig::default(),
            portal_only: false,
        }
    }

    #[test]
    fn portal_only_flag_does_not_consume_an_option_value() {
        let args = [
            "--portal-only",
            "--listen",
            "127.0.0.1:8081",
            "--socket",
            "/tmp/control",
        ]
        .map(String::from);
        let (options, portal_only) = parse_options(&args, "serve").unwrap();
        assert!(portal_only);
        assert_eq!(options["--listen"], "127.0.0.1:8081");
        assert_eq!(options["--socket"], "/tmp/control");
        assert!(parse_options(&["--portal-only".into(), "--portal-only".into()], "serve").is_err());
        assert!(parse_options(&["--portal-only".into()], "attach").is_err());
        assert!(parse_options(&["--listen".into()], "serve").is_err());
    }

    #[test]
    fn management_configuration_fails_closed_and_has_bounded_defaults() {
        let mut config = portal_config_fixture();
        assert_eq!(config.portal.audit_retention_days, 7);
        assert_eq!(config.portal.audit_max_events, 20000);
        validate_portal_config(&config).unwrap();
        config.portal.admin_github_ids.push("123".into());
        assert!(validate_portal_config(&config).is_err());
        config.portal.gateway_socket = Some("/tmp/management.sock".into());
        assert!(
            validate_portal_config(&config).is_err(),
            "durable audit directory is required"
        );
        config.portal.audit_dir = Some("/tmp/audit".into());
        validate_portal_config(&config).unwrap();
        config.portal.admin_github_ids = vec!["01".into()];
        assert!(validate_portal_config(&config).is_err());
        config.portal.admin_github_ids = vec!["123".into()];
        config
            .portal
            .owner_mappings
            .insert("gh-123".into(), "456".into());
        assert!(
            validate_portal_config(&config).is_err(),
            "immutable ownership cannot be overridden"
        );
        config.portal.owner_mappings.clear();
        config.portal.audit_retention_days = 0;
        assert!(validate_portal_config(&config).is_err());
        config.portal.audit_retention_days = 7;
        config.portal.audit_max_events = 100001;
        assert!(validate_portal_config(&config).is_err());
        config.portal.audit_max_events = 20000;
        config.portal.gateway_socket = Some("/tmp/../management.sock".into());
        assert!(validate_portal_config(&config).is_err());
        config.portal.gateway_socket = Some(config.socket.clone());
        assert!(validate_portal_config(&config).is_err());
    }

    #[test]
    fn authorization_config_cannot_be_group_or_world_writable_without_provider_secrets() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ssmcp-policy-{}-{nonce}.toml", std::process::id()));
        std::fs::write(&path, "[portal]\nadmin_github_ids = ['123']\n").unwrap();
        let mut config = portal_config_fixture();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        load_file_config(&path, &mut config).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(
            load_file_config(&path, &mut config)
                .unwrap_err()
                .to_string()
                .contains("not group/world writable")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn portal_only_requires_account_storage_and_never_needs_its_own_audit_store() {
        let mut config = portal_config_fixture();
        config.portal_only = true;
        assert!(validate_portal_config(&config).is_err());
        config.portal.state_dir = Some("/tmp/accounts".into());
        config.portal.gateway_socket = Some("/tmp/management.sock".into());
        validate_portal_config(&config).unwrap();
    }

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
            client_registry: None,
            portal: PortalConfig::default(),
            portal_only: false,
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
