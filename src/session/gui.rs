//! Explicit CLI-owned gateway for web, desktop and mobile frontends.
mod gateway;

use gateway::{Connection, CoreFuture, Host};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{Notify, RwLock},
};
use zbridge::{BridgeError, ErrorCode, Request, Response};

use super::attach::RemoteFrontend;

const HELP: &str = "ssmcp gui open
ssmcp gui serve [--listen 127.0.0.1:PORT] [--assets DIRECTORY]\n                [--pairing-file PRIVATE_FILE] [--public-origin https://HOST]\n\nOpen starts the local workbench in your browser or reopens the running one.\nServe hosts the built-in web app; --assets selects a replacement directory.\nConnects frontends to existing saved CLI sessions. It does not start a model or\nsession. The listener is loopback only. For mobile access, explicitly configure\nan HTTPS reverse proxy preserving the public Host and Origin. Pairing credentials\nare written to a private local JSON file and are never returned by HTTP.\n";

#[derive(Default)]
struct LocalHost {
    foreground_root: Option<PathBuf>,
}

impl Host for LocalHost {
    fn sessions(&self) -> CoreFuture<'_, Vec<Value>> {
        Box::pin(async {
            match &self.foreground_root {
                Some(root) => super::overview::foreground_rows_in(root).map_err(native),
                None => super::local_rows().await.map_err(native),
            }
        })
    }
    fn connect(&self, session: String, read_only: bool) -> CoreFuture<'_, Arc<dyn Connection>> {
        Box::pin(async move {
            let root = match &self.foreground_root {
                Some(root) => root.clone(),
                None => super::session_root(false).map_err(native)?,
            };
            let (directory, pid) = if session.starts_with("live:") {
                let (directory, _, pid) =
                    super::overview::foreground_target_in(&root, &session).map_err(native)?;
                (directory, Some(pid))
            } else {
                if self.foreground_root.is_some() {
                    return Err(BridgeError::invalid("expected a foreground session target"));
                }
                let name = session.strip_prefix("saved:").unwrap_or(&session);
                super::validate_name(name)
                    .map_err(|_| BridgeError::invalid("invalid saved session name"))?;
                (super::load(&root, name).map_err(native)?.0, None)
            };
            let frontend = RemoteFrontend::connect_for_pid(directory, read_only, pid)
                .await
                .map_err(native)?;
            Ok(Arc::new(Attached::new(frontend)) as Arc<dyn Connection>)
        })
    }
}

#[cfg(any(target_os = "android", all(test, unix)))]
pub(crate) fn foreground_workbench(
    root: PathBuf,
    origin: String,
    token: String,
) -> crate::Result<axum::Router> {
    super::create_private_tree(&root)?;
    super::verify_directory(&root)?;
    Ok(gateway::routes(
        gateway::Config {
            token,
            origin,
            assets: None,
        },
        Arc::new(LocalHost {
            foreground_root: Some(root),
        }),
        true,
    )?)
}
fn native(error: crate::Error) -> BridgeError {
    BridgeError::new(ErrorCode::Unavailable, error.to_string())
}

struct Attached {
    frontend: RwLock<RemoteFrontend>,
    closed: AtomicBool,
    closing: Notify,
}
impl Attached {
    fn new(frontend: RemoteFrontend) -> Self {
        Self {
            frontend: RwLock::new(frontend),
            closed: AtomicBool::new(false),
            closing: Notify::new(),
        }
    }
}
impl Connection for Attached {
    fn request(&self, request: Request) -> CoreFuture<'_, Response> {
        Box::pin(async move {
            let command = request.operation.is_command();
            let closing = self.closing.notified();
            tokio::pin!(closing);
            closing.as_mut().enable();
            if self.closed.load(Ordering::Acquire) {
                return Err(BridgeError::new(
                    ErrorCode::Closed,
                    "gateway connection was detached",
                ));
            }
            let frontend = tokio::select! {
                biased;
                _ = &mut closing => return Err(BridgeError::new(ErrorCode::Closed, "gateway connection was detached")),
                frontend = self.frontend.read() => frontend,
            };
            if self.closed.load(Ordering::Acquire) {
                return Err(BridgeError::new(
                    ErrorCode::Closed,
                    "gateway connection was detached",
                ));
            }
            // Reads share the native frontend. No lock spans an entire 55-second
            // observation window; closing also cancels an in-flight IPC wait.
            let response = tokio::select! {
                biased;
                _ = &mut closing => Err(BridgeError::new(ErrorCode::Closed, "gateway connection is detaching")),
                response = frontend.request(request) => response,
            };
            if self.closed.load(Ordering::Acquire) {
                let error = BridgeError::new(
                    ErrorCode::Closed,
                    "gateway connection detached during the native request",
                );
                return Err(if command { error.uncertain() } else { error });
            }
            response
        })
    }
    fn close(&self) -> CoreFuture<'_, ()> {
        Box::pin(async move {
            // RemoteFrontend::close is idempotent only after its real detach
            // acknowledgement. Preserve the lease after failure for an explicit
            // DELETE retry instead of claiming an unobserved release succeeded.
            self.closed.store(true, Ordering::Release);
            self.closing.notify_waiters();
            self.frontend.write().await.close().await.map_err(native)
        })
    }
}

struct PairingFile {
    path: PathBuf,
    token: String,
}
impl Drop for PairingFile {
    fn drop(&mut self) {
        if super::read_json::<Value>(&self.path).is_ok_and(|value| {
            value["token"]
                .as_str()
                .is_some_and(|token| bool::from(token.as_bytes().ct_eq(self.token.as_bytes())))
        }) {
            let _ = super::remove_file(&self.path);
        }
    }
}

/// Run an explicitly requested GUI gateway without starting sessions or models.
pub async fn run(arguments: Vec<OsString>) -> crate::Result<()> {
    let arguments: Vec<String> = arguments
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "gui arguments must be UTF-8".into())
        })
        .collect::<crate::Result<_>>()?;
    if arguments.is_empty()
        || arguments
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print!("{HELP}");
        return Ok(());
    }
    let open_browser = arguments[0] == "open" && arguments.len() == 1;
    if !open_browser && arguments[0] != "serve" {
        return Err(HELP.into());
    }
    let mut listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let mut assets = None;
    let mut pairing = None;
    let mut public_origin = None;
    let (options, remainder) = arguments[1..].as_chunks::<2>();
    for pair in options {
        match pair[0].as_str() {
            "--listen" => {
                listen = pair[1]
                    .parse()
                    .map_err(|_| "--listen requires a loopback IP:port")?
            }
            "--assets" => assets = Some(PathBuf::from(&pair[1]).canonicalize()?),
            "--pairing-file" => pairing = Some(PathBuf::from(&pair[1])),
            "--public-origin" => {
                let url = reqwest::Url::parse(&pair[1])?;
                if url.scheme() != "https" {
                    return Err("--public-origin requires an explicit HTTPS origin".into());
                }
                public_origin = Some(pair[1].clone());
            }
            _ => return Err(format!("unknown gui option {}\n{HELP}", pair[0]).into()),
        }
    }
    if !remainder.is_empty() {
        return Err("gui options require values".into());
    }
    if !listen.ip().is_loopback() {
        return Err("GUI gateway binds only loopback; configure an explicit HTTPS reverse proxy for other devices".into());
    }
    let directory = super::state_path()?.join("gui");
    super::create_private_tree(&directory)?;
    super::verify_directory(&directory)?;
    let pairing = pairing.unwrap_or_else(|| directory.join("pairing.json"));
    let lock = super::lease_file(&directory.join("gateway.lock"))?;
    if let Err(error) = crate::platform::lock_file(&lock, true) {
        if open_browser && error.kind() == std::io::ErrorKind::WouldBlock {
            let current: Value = super::read_json(&pairing)?;
            let origin = current["url"].as_str().ok_or("missing gateway URL")?;
            let url = reqwest::Url::parse(origin)?;
            if current["schema"] != "ssmcp-gui-pairing-v1"
                || !matches!(url.scheme(), "http" | "https")
                || url.as_str() != format!("{}/", url.origin().ascii_serialization())
            {
                return Err("invalid gateway pairing file".into());
            }
            return crate::client::account::open_browser(&url).await;
        }
        return Err(error.into());
    }
    if !pairing.is_absolute() {
        return Err("--pairing-file must be an absolute path in a private directory".into());
    }
    let parent = pairing.parent().ok_or("pairing file has no parent")?;
    super::verify_directory(parent)?;
    if let Some(assets) = &assets
        && parent.canonicalize()?.starts_with(assets)
    {
        return Err("pairing credentials must stay outside the public assets directory".into());
    }
    if pairing.try_exists()? {
        let previous: Value = super::read_json(&pairing)?;
        if previous["schema"] != "ssmcp-gui-pairing-v1" {
            return Err("existing pairing file belongs to another format".into());
        }
    }
    let listener = TcpListener::bind(listen).await?;
    let address = listener.local_addr()?;
    let origin = public_origin.unwrap_or_else(|| format!("http://{address}"));
    let mut secret = [0_u8; 32];
    crate::platform::random_bytes(&mut secret)?;
    let token = crate::hex::encode(secret);
    let app = gateway::router(
        gateway::Config {
            token: token.clone(),
            origin: origin.clone(),
            assets: assets.clone(),
        },
        Arc::new(LocalHost::default()),
    )?;
    super::write_json(
        &pairing,
        &json!({"schema":"ssmcp-gui-pairing-v1","url":origin,"token":token,"pid":std::process::id(),"lifetime":"this_gateway_process"}),
    )?;
    let _pairing = PairingFile {
        path: pairing.clone(),
        token,
    };
    println!(
        "{}",
        json!({"service":"ssmcp-gui","listener":address,"url":origin,"pairing_file":pairing,"assets":assets})
    );
    if open_browser {
        crate::client::account::open_browser(&reqwest::Url::parse(&origin)?).await?;
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

pub(crate) async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(all(test, unix))]
#[path = "gui/tests.rs"]
mod tests;

#[cfg(all(test, unix))]
pub(crate) use tests::exercise_gateway_fixture;
