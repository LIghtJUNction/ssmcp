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
    io::{self, Read},
    net::SocketAddr,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot};

const SOCKET: &str = "/run/ssmcpd/control.sock";
const CONFIG_FILE: &str = "/etc/ssmcpd/ssmcpd.toml";
const HELP: &str = r#"ssmcpd - OAuth-authenticated HTTPS terminal MCP gateway

Usage:
  ssmcpd serve [--config PATH] [--public-url https://HOST] [--listen 127.0.0.1:8080] [--portal-only]
  ssmcpd check-runtime

Runtime check:
  check-runtime verifies the installed code_mode runner and mandatory OS sandbox.
  It takes no options and needs no gateway configuration, credentials, or network.
  Success prints {"code_mode":"ok"}; failure exits nonzero with a diagnostic.

Configuration:
  --config PATH selects a TOML file; SSMCP_CONFIG also selects a file.
  Otherwise /etc/ssmcpd/ssmcpd.toml is read when present.
  Precedence: command line > environment > TOML > built-in defaults.
  Portal: --portal-state-dir PATH overrides [portal] state_dir (private persistent directory).
  --portal-only runs the account HTTP portal without running the gateway management service.
  --gateway-socket PATH overrides [portal] gateway_socket (private gateway management socket).
  Management: [portal] admin_usernames = ["your-ssmcp-username"] after registering that name.
  Relay bandwidth: [portal] default_rate_limit_bps = 8388608 (8 MiB/s in each direction per user).
  Legacy configuration: admin_github_ids, admin_github_users, owner_mappings.
  Admin IDs are immutable GitHub numeric ID strings; username entries resolve once at startup.
  Audit: [portal] audit_dir, audit_retention_days (default 7), audit_max_events (default 20000).
  The gateway requires audit_dir when gateway_socket is configured. Portal-only is a socket client.
  Server settings: [server] listen, public_url, socket, client_download_dir.
  --client-download-dir PATH serves native binaries for temporary device commands.
  Files: ssmcp-TARGET (Windows: ssmcp-TARGET.exe), from the matching release.
  Environment: SSMCP_LISTEN, SSMCP_PUBLIC_URL, SSMCP_SOCKET, SSMCP_CLIENT_DOWNLOAD_DIR.
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

Run serve in the foreground under systemd.
"#;

struct Config {
    listen: SocketAddr,
    public_url: String,
    socket: PathBuf,
    oauth: OAuthConfig,
    github: Option<GitHubConfig>,
    client_registry: Option<PathBuf>,
    client_download_dir: Option<PathBuf>,
    portal: PortalConfig,
    commerce: ssmcp::commerce::CommerceConfig,
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
    commerce: ssmcp::commerce::CommerceConfig,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PortalConfig {
    state_dir: Option<PathBuf>,
    gateway_socket: Option<PathBuf>,
    admin_github_ids: Vec<String>,
    admin_github_users: Vec<String>,
    admin_usernames: Vec<String>,
    default_rate_limit_bps: u64,
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
            admin_usernames: Vec::new(),
            default_rate_limit_bps: ssmcp::user_policy::DEFAULT_RATE_LIMIT_BPS,
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
    client_download_dir: Option<PathBuf>,
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
    config.client_download_dir = file.server.client_download_dir;
    config.portal = file.portal;
    config.commerce = file.commerce;
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
    let remaining: Vec<String> = args.collect();
    if mode == "check-runtime" {
        if remaining.len() == 1 && matches!(remaining[0].as_str(), "--help" | "-h") {
            print!("{HELP}");
            return Ok(());
        }
        if !remaining.is_empty() {
            return Err("check-runtime takes no options or arguments; see --help".into());
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(ssmcp::code_mode::check_runtime())?;
        println!(r#"{{"code_mode":"ok"}}"#);
        return Ok(());
    }
    if mode != "serve" {
        return Err(format!("unknown command {mode}; see --help").into());
    }
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
        client_download_dir: None,
        portal: PortalConfig::default(),
        commerce: ssmcp::commerce::CommerceConfig::default(),
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
        if !options.contains_key("--client-download-dir")
            && let Some(value) = std::env::var_os("SSMCP_CLIENT_DOWNLOAD_DIR")
        {
            config.client_download_dir = Some(value.into());
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
            "--client-download-dir" if mode == "serve" => {
                config.client_download_dir = Some(value.into())
            }
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
    config.commerce.validate()?;
    if config.commerce.enabled && (config.portal_only || portal.state_dir.is_none()) {
        return Err("commerce requires the gateway process with portal.state_dir; --portal-only cannot own billing".into());
    }
    if !ssmcp::user_policy::valid_rate(portal.default_rate_limit_bps) {
        return Err(
            "portal.default_rate_limit_bps must be in 1..=1073741824 bytes per second".into(),
        );
    }
    if !portal.admin_usernames.is_empty() && portal.state_dir.is_none() {
        return Err(
            "portal.admin_usernames requires portal.state_dir and registered usernames".into(),
        );
    }
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
                || !portal.admin_usernames.is_empty()
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

async fn serve(config: Config) -> ssmcp::Result<()> {
    use std::future::IntoFuture;
    let (downloads, publication) = ssmcp::client_downloads::configured(
        &config.public_url,
        config.client_download_dir.as_deref(),
    )?;
    let mut broker = Broker::with_identity_config(config.public_url, config.oauth, config.github)?;
    broker.client_downloads = publication;
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

    broker
        .configure_traffic_timezone(config.commerce.utc_offset_minutes)
        .map_err(|_| "invalid commerce monthly timezone")?;
    if let Some(path) = &config.portal.state_dir {
        broker
            .portal
            .configure_commerce(config.commerce.clone(), path)?;
    }
    broker
        .configure_user_policies(config.portal.default_rate_limit_bps)
        .await?;

    // A portal-only process never opens audit storage or binds either gateway socket.
    let mut audit = None;
    let management_server = if !config.portal_only
        && let Some(socket) = &config.portal.gateway_socket
    {
        let mut admins = config.portal.admin_github_ids;
        admins.extend(
            broker
                .portal
                .resolve_admin_usernames(&config.portal.admin_usernames)
                .await?,
        );
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
    broker.reconcile_commerce().await?;
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
    let server = axum::serve(tcp, http::router(broker.clone()).merge(downloads))
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .into_future();
    tokio::pin!(server);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
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
            _ = prune.tick(), if audit.is_some() => {
                let log = audit.as_ref().expect("guarded audit timer").clone();
                if !matches!(tokio::task::spawn_blocking(move || log.prune()).await, Ok(Ok(_))) {
                    ssmcp::observability::log("error", "persistence", &[("event", "audit_prune_failed".into())]);
                }
            }
            result = &mut interrupt => { result?; break Ok(()); }
            _ = terminate.recv() => break Ok(()),
        }
    };
    broker.disconnect_all().await;
    let _ = stop_tx.send(());
    if !server_finished {
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut server).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn commerce_defaults_keep_sales_closed_at_the_requested_rmb_price() {
        let config =
            parse_file_config("[commerce]\nenabled = false\nmonthly_sellable_gb = 0\n").unwrap();
        assert!(!config.commerce.enabled);
        assert_eq!(config.commerce.currency, "CNY");
        assert_eq!(config.commerce.price_per_gb_minor, Some(100));
        assert_eq!(config.commerce.monthly_sellable_gb, 0);
        assert_eq!(config.commerce.utc_offset_minutes, 480);
        assert!(parse_file_config("[commerce]\nprice_per_gb_minro = 1\n").is_err());
    }

    #[test]
    fn commerce_rejects_a_second_portal_only_billing_writer() {
        let mut config = portal_config_fixture();
        config.portal_only = true;
        config.portal.state_dir = Some("/tmp/accounts".into());
        config.commerce.enabled = true;
        config.commerce.merchant_id = "MER_test".into();
        config.commerce.store_id = "STO_test".into();
        config.commerce.product_id = "PROD_test".into();
        config.commerce.checkout_origin = "https://pancake.waffo.ai".into();
        config.commerce.private_key_file = Some("/tmp/test-private.pem".into());
        config.commerce.webhook_public_key_file = Some("/tmp/test-public.pem".into());
        assert!(config.commerce.validate().is_ok());
        assert!(validate_portal_config(&config).is_err());
    }

    fn portal_config_fixture() -> Config {
        Config {
            listen: "127.0.0.1:8080".parse().unwrap(),
            public_url: "https://ssmcp.test".into(),
            socket: "/tmp/control.sock".into(),
            oauth: OAuthConfig::default(),
            github: None,
            client_registry: None,
            client_download_dir: None,
            portal: PortalConfig::default(),
            commerce: ssmcp::commerce::CommerceConfig::default(),
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
            client_download_dir: None,
            portal: PortalConfig::default(),
            commerce: ssmcp::commerce::CommerceConfig::default(),
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
