//! Browser primitives implemented directly with the Chrome DevTools Protocol.
//! The browser is a system dependency; no model or external MCP process is involved.
mod discovery;
use crate::backend::{process::Managed, result};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, OnceCell, OwnedMutexGuard},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

const RPC_LIMIT: usize = 2 * 1024 * 1024;
const JSON_LIMIT: usize = 1024 * 1024;
const IMAGE_LIMIT: usize = 400 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const INPUT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PAGES: usize = 64;

/// Observe installed browser metadata without creating a browser runtime.
pub fn discovery() -> Value {
    discovery::inspect()
}

pub struct Browser {
    root: PathBuf,
    http: OnceCell<reqwest::Client>,
    session: Mutex<Option<Arc<Session>>>,
    closed: AtomicBool,
    next_snapshot: AtomicU64,
}

struct Session {
    endpoint: Url,
    browser: Cdp,
    pages: StdMutex<HashMap<String, Arc<PageEntry>>>,
    owned: StdMutex<Option<OwnedBrowser>>,
    version: Value,
    closed: AtomicBool,
}

/// A profile is removed only after its owned process has exited.
struct Profile(PathBuf);
impl Drop for Profile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct OwnedBrowser {
    process: Option<Managed>,
    profile: Option<Profile>,
}
impl OwnedBrowser {
    async fn shutdown(&mut self) {
        if let Some(process) = self.process.as_mut() {
            process.shutdown().await;
        }
        self.process.take();
        self.profile.take();
    }
}
impl Drop for OwnedBrowser {
    fn drop(&mut self) {
        let Some(mut process) = self.process.take() else {
            return;
        };
        let profile = self.profile.take();
        process.terminate();
        // This reaper does not depend on the caller future or Tokio runtime
        // surviving cancellation. Managed's guardian owns the full process tree.
        std::thread::spawn(move || {
            while matches!(process.child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(process);
            drop(profile);
        });
    }
}

// Initializing or operating one page must not block an unrelated page.
struct PageEntry {
    connection: OnceCell<PageConnection>,
    closed: AtomicBool,
}
// Removing a cancelled first connection must not need an async runtime or
// leave a placeholder consuming MAX_PAGES. Only the initializer owns this guard.
struct InitializingPage<'a> {
    session: &'a Session,
    id: &'a str,
    entry: &'a Arc<PageEntry>,
    armed: bool,
}
impl Drop for InitializingPage<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut pages = self.session.pages.lock().unwrap_or_else(|e| e.into_inner());
            if pages
                .get(self.id)
                .is_some_and(|entry| Arc::ptr_eq(entry, self.entry))
            {
                self.entry.closed.store(true, Ordering::Release);
                pages.remove(self.id);
            }
        }
    }
}
struct PageConnection {
    // Kept outside the page operation lock so disconnect can drain input and
    // close sockets even when an operation has been cancelled while awaiting.
    cdp: Cdp,
    page: Arc<Mutex<Page>>,
}
struct Page {
    cdp: Cdp,
    snapshot: Option<Snapshot>,
}
struct Snapshot {
    token: String,
    object_id: String,
    count: usize,
}
#[derive(Clone)]
struct Cdp {
    connection: Arc<Mutex<Connection>>,
    url: String,
}
struct Connection {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    sequence: u64,
    closed: bool,
    focus_emulated: bool,
}
impl Cdp {
    async fn connect(url: &str) -> Result<Self, String> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(RPC_LIMIT))
            .max_frame_size(Some(RPC_LIMIT));
        let (socket, _) = tokio::time::timeout(
            CALL_TIMEOUT,
            connect_async_with_config(url, Some(config), true),
        )
        .await
        .map_err(|_| "CDP WebSocket connection timed out".to_owned())?
        .map_err(|e| format!("CDP WebSocket connection failed: {e}"))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(Connection {
                socket,
                sequence: 0,
                closed: false,
                focus_emulated: false,
            })),
            url: url.to_owned(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        // Include queue time: an abandoned caller may still be releasing input.
        tokio::time::timeout(CALL_TIMEOUT, async {
            self.connection
                .lock()
                .await
                .call(method, params, CALL_TIMEOUT)
                .await
        })
        .await
        .map_err(|_| format!("CDP {method} timed out"))?
    }

    /// Chromium can acknowledge trusted input to a hidden tab without delivering
    /// it. Emulate focus only inside this explicit input target; never activate a
    /// tab or focus an OS window. Keep rendering active until this CDP connection
    /// closes, including while waiting for asynchronous results.
    async fn prepare_input(&self) -> Result<(), String> {
        tokio::time::timeout(CALL_TIMEOUT, async {
            let mut connection = self.connection.lock().await;
            // Mark before sending: a cancelled request may already have applied.
            // Re-send on every input rather than assuming an interrupted request
            // succeeded; the marker is used only for unconditional restoration.
            connection.focus_emulated = true;
            connection
                .call(
                    "Emulation.setFocusEmulationEnabled",
                    json!({"enabled":true}),
                    CALL_TIMEOUT,
                )
                .await
                .map(|_| ())
        })
        .await
        .map_err(|_| "browser input focus emulation timed out".to_owned())?
    }

    async fn input_pair(&self, method: &str, press: Value, release: Value) -> Result<(), String> {
        let cdp = self.clone();
        let method = method.to_owned();
        // Once a press starts, cancellation of the requesting MCP call must not
        // discard its release. The task owns the connection through both calls.
        tokio::spawn(async move {
            let mut connection = tokio::time::timeout(CALL_TIMEOUT, cdp.connection.lock())
                .await
                .map_err(|_| "CDP input queue timed out".to_owned())?;
            let pressed = connection.call(&method, press, INPUT_TIMEOUT).await;
            let mut released = connection
                .call(&method, release.clone(), INPUT_TIMEOUT)
                .await;
            if released.is_err() {
                // The press may have reached Chromium before a connection broke.
                // Reconnect to that same verified target for one bounded release.
                let focus_emulated = connection.focus_emulated;
                released = match tokio::time::timeout(Duration::from_secs(6), async {
                    let fresh = Cdp::connect(&cdp.url).await?;
                    if focus_emulated {
                        fresh.prepare_input().await?;
                    }
                    let released = fresh.call(&method, release).await;
                    fresh.close().await;
                    released
                })
                .await
                {
                    Ok(value) => value,
                    Err(_) => Err("CDP input release recovery timed out".into()),
                };
            }
            if let Err(error) = released {
                return Err(format!("CDP input release failed: {error}"));
            }
            pressed.map(|_| ())
        })
        .await
        .map_err(|e| format!("CDP input task failed: {e}"))?
    }

    async fn close(&self) {
        // Includes waiting for an owned press/release pair, then bounds the
        // close write itself. Dropping the connection never waits for a peer.
        let _ = tokio::time::timeout(Duration::from_secs(35), async {
            let mut connection = self.connection.lock().await;
            if connection.focus_emulated && !connection.closed {
                let _ = connection
                    .call(
                        "Emulation.setFocusEmulationEnabled",
                        json!({"enabled":false}),
                        Duration::from_secs(2),
                    )
                    .await;
                connection.focus_emulated = false;
            }
            connection.closed = true;
            let _ =
                tokio::time::timeout(Duration::from_secs(2), connection.socket.close(None)).await;
        })
        .await;
    }

    async fn value(&self, expression: &str) -> Result<Value, String> {
        let reply = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression":expression,"returnByValue":true,"awaitPromise":true,"timeout":10000
                }),
            )
            .await?;
        remote_value(reply)
    }
}
impl Connection {
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.closed {
            return Err("CDP connection is closed".into());
        }
        self.sequence += 1;
        let id = self.sequence;
        let request = json!({"id":id,"method":method,"params":params}).to_string();
        tokio::time::timeout(timeout, async {
            self.socket
                .send(Message::Text(request.into()))
                .await
                .map_err(|e| format!("CDP {method}: {e}"))?;
            while let Some(message) = self.socket.next().await {
                let message = message.map_err(|e| format!("CDP {method}: {e}"))?;
                let text = match message {
                    Message::Text(text) => text,
                    Message::Ping(data) => {
                        self.socket
                            .send(Message::Pong(data))
                            .await
                            .map_err(|e| format!("CDP pong: {e}"))?;
                        continue;
                    }
                    Message::Close(_) => return Err(format!("CDP disconnected during {method}")),
                    _ => continue,
                };
                let reply: Value = serde_json::from_str(&text)
                    .map_err(|e| format!("invalid CDP response: {e}"))?;
                // Events and replies from a previously timed-out call are not this result.
                if reply["id"].as_u64() != Some(id) {
                    continue;
                }
                if let Some(error) = reply.get("error") {
                    return Err(format!("CDP {method}: {error}"));
                }
                return reply
                    .get("result")
                    .cloned()
                    .ok_or_else(|| format!("CDP {method} returned no result"));
            }
            Err(format!("CDP connection ended during {method}"))
        })
        .await
        .map_err(|_| format!("CDP {method} timed out"))?
    }
}

fn remote_value(reply: Value) -> Result<Value, String> {
    remote_value_bounded(reply, 128 * 1024)
}
fn remote_value_bounded(reply: Value, limit: usize) -> Result<Value, String> {
    if let Some(exception) = reply.get("exceptionDetails") {
        return Err(format!("browser JavaScript exception: {exception}"));
    }
    if let Some(value) = reply["result"].get("value") {
        if value.to_string().len() > limit {
            return Err(format!(
                "browser result exceeds {limit} bytes; request a smaller value"
            ));
        }
        return Ok(value.clone());
    }
    if let Some(value) = reply["result"].get("unserializableValue") {
        return Ok(json!({"type":reply["result"]["type"],"unserializable_value":value}));
    }
    if reply["result"]["type"] == "undefined" {
        return Ok(Value::Null);
    }
    Err("browser evaluation did not return a serializable value".into())
}

impl Browser {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("browser workspace: {e}"))?;
        Ok(Self {
            root,
            http: OnceCell::new(),
            session: Mutex::new(None),
            closed: AtomicBool::new(false),
            next_snapshot: AtomicU64::new(1),
        })
    }

    pub fn tools() -> Vec<Value> {
        let target = json!({"target_id":{"type":"string","minLength":1,"maxLength":128}});
        let target_ref = json!({
            "target_id":{"type":"string","minLength":1,"maxLength":128},
            "ref":{"type":"string","minLength":1,"maxLength":80}
        });
        let mut out = vec![
            tool(
                "discover",
                "Read bounded browser process and extension-manifest metadata. Distinguish Google Chrome from Chromium; installed files do not prove enabled extensions, native approval or a usable connection. No browser, MCP server or installer is started.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "status",
                "Inspect native browser availability and the current CDP connection; no browser is started.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "launch",
                "Start an installed Chromium browser with an isolated temporary profile, loopback debugging, and background surface capture. Headless is the default; headless:false explicitly opens a desktop window. Disconnect stops only this owned browser.",
                json!({
                    "executable":{"type":"string","maxLength":4096},
                    "headless":{"type":"boolean","default":true},
                    "width":{"type":"integer","minimum":320,"maximum":4096,"default":1280},
                    "height":{"type":"integer","minimum":240,"maximum":4096,"default":800}
                }),
                &[],
                false,
            ),
            tool(
                "connect",
                "Attach to an explicitly supplied local Chromium CDP HTTP endpoint, for example http://127.0.0.1:9222. Disconnect never terminates an attached browser.",
                json!({
                    "endpoint":{"type":"string","maxLength":2048}
                }),
                &["endpoint"],
                false,
            ),
            tool(
                "tabs",
                "List page target IDs, titles and URLs from the connected browser.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "open",
                "Open a new browser tab in the background and return its target_id. Set background=false only when foreground activation is intended.",
                json!({"url":{"type":"string","maxLength":32768},"background":{"type":"boolean","default":true}}),
                &["url"],
                false,
            ),
            tool(
                "navigate",
                "Navigate one observed target. Clears previous element refs and waits until the document is interactive.",
                merge(
                    target.clone(),
                    json!({"url":{"type":"string","maxLength":32768}}),
                ),
                &["target_id", "url"],
                false,
            ),
            tool(
                "snapshot",
                "Read bounded visible page text and interactive elements. Returned refs belong to this snapshot and target; take a new snapshot after navigation or DOM replacement. Password values are omitted.",
                merge(
                    target.clone(),
                    json!({
                        "max_elements":{"type":"integer","minimum":1,"maximum":200,"default":100},
                        "max_chars":{"type":"integer","minimum":1,"maximum":32768,"default":12000}
                    }),
                ),
                &["target_id"],
                true,
            ),
            tool(
                "click",
                "Click an element ref from the latest snapshot using native browser mouse events; reject detached, obscured or stale elements.",
                merge(
                    target_ref.clone(),
                    json!({
                        "button":{"type":"string","enum":["left","right","middle"],"default":"left"},
                        "click_count":{"type":"integer","minimum":1,"maximum":3,"default":1}
                    }),
                ),
                &["target_id", "ref"],
                false,
            ),
            tool(
                "fill",
                "Replace the value of an editable element ref, including Unicode text, using the browser input protocol.",
                merge(
                    target_ref,
                    json!({"text":{"type":"string","maxLength":32768}}),
                ),
                &["target_id", "ref", "text"],
                false,
            ),
            tool(
                "key",
                "Press and release a key in the selected tab. Use Enter, Tab, Escape, arrows, editing keys, or one character, with optional modifiers.",
                merge(
                    target.clone(),
                    json!({
                        "key":{"type":"string","minLength":1,"maxLength":32},
                        "modifiers":{"type":"array","maxItems":4,"uniqueItems":true,"items":{"type":"string","enum":["Alt","Control","Meta","Shift"]}}
                    }),
                ),
                &["target_id", "key"],
                false,
            ),
            tool(
                "scroll",
                "Scroll the selected viewport by CSS pixels using browser wheel events.",
                merge(
                    target.clone(),
                    json!({
                        "delta_x":{"type":"integer","minimum":-100000,"maximum":100000,"default":0},
                        "delta_y":{"type":"integer","minimum":-100000,"maximum":100000,"default":0}
                    }),
                ),
                &["target_id"],
                false,
            ),
            tool(
                "wait_for",
                "Wait on this client for visible page text to appear or disappear without moving the system pointer or repeatedly transferring snapshots. Bounded timeout; holds this target's action order.",
                merge(
                    target.clone(),
                    json!({
                        "text":{"type":"string","minLength":1,"maxLength":1024},
                        "state":{"type":"string","enum":["present","absent"],"default":"present"},
                        "timeout_ms":{"type":"integer","minimum":1,"maximum":30000,"default":5000}
                    }),
                ),
                &["target_id", "text"],
                true,
            ),
            tool(
                "evaluate",
                "Evaluate explicit JavaScript in the selected page; return a JSON value bounded to 128 KiB. May change page state.",
                merge(
                    target.clone(),
                    json!({"expression":{"type":"string","maxLength":65536}}),
                ),
                &["target_id", "expression"],
                false,
            ),
            tool(
                "screenshot",
                "Capture the current browser viewport as a bounded MCP JPEG image plus pixel dimensions. Use image(await browser.screenshot(...)) in code_mode to display it.",
                merge(
                    target.clone(),
                    json!({"max_width":{"type":"integer","minimum":320,"maximum":2560,"default":1280}}),
                ),
                &["target_id"],
                true,
            ),
            tool(
                "close",
                "Close one explicitly selected tab.",
                target,
                &["target_id"],
                false,
            ),
            tool(
                "disconnect",
                "Release the CDP connection; stop and remove the temporary profile only when ssmcp launched this browser.",
                json!({}),
                &[],
                false,
            ),
        ];
        for descriptor in &mut out {
            descriptor["annotations"]["openWorldHint"] = json!(true);
            descriptor["_meta"]["ssmcp/input_scope"] = json!("browser_target");
            descriptor["_meta"]["ssmcp/moves_system_pointer"] = json!(false);
        }
        out
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.closed.load(Ordering::Acquire) {
            Err("browser runtime is closed".into())
        } else {
            Ok(())
        }
    }

    pub async fn perform(&self, name: &str, args: Value) -> Result<Value, String> {
        self.ensure_open()?;
        match name {
            "ssmcp.browser.discover" => {
                let _: Empty = parse(args)?;
                let observed = tokio::task::spawn_blocking(discovery::inspect)
                    .await
                    .map_err(|_| "browser metadata discovery failed".to_owned())?;
                self.ensure_open()?;
                Ok(result(observed))
            }
            "ssmcp.browser.status" => {
                let _: Empty = parse(args)?;
                let session = self.session.lock().await.clone();
                self.ensure_open()?;
                let mut state = if let Some(s) = session.as_ref() {
                    let probe = tokio::time::timeout(Duration::from_secs(2), async {
                        let discovery = self.get_json(&s.endpoint, "json/version").await?;
                        let ws = websocket(
                            &s.endpoint,
                            discovery["webSocketDebuggerUrl"]
                                .as_str()
                                .ok_or("browser debugger has no WebSocket URL")?,
                        )?;
                        if ws.as_str() != s.browser.url {
                            return Err(
                                "browser debugger identity changed; disconnect and reconnect"
                                    .to_owned(),
                            );
                        }
                        if s.closed.load(Ordering::Acquire) {
                            return Err("browser disconnected during status".to_owned());
                        }
                        Ok::<(), String>(())
                    })
                    .await
                    .map_err(|_| "browser debugger status probe timed out".to_owned())
                    .and_then(|v| v);
                    json!({"connected":probe.is_ok(),"endpoint":s.endpoint.as_str(),
                        "owned":s.owned.lock().unwrap_or_else(|e|e.into_inner()).is_some(),"browser":s.version,"native":true,"protocol":"cdp",
                        "reason":probe.err()})
                } else {
                    json!({"connected":false,"native":true,"protocol":"cdp",
                        "executable":installed_browser(None).ok(),"connect_supported":true})
                };
                state["input_scope"] = json!("browser_target");
                state["moves_system_pointer"] = json!(false);
                state["default_new_tab_background"] = json!(true);
                state["default_launch_headless"] = json!(true);
                state["background_screenshot"] = json!(
                    "owned launch enables background surface capture; attached browsers depend on their startup configuration"
                );
                state["page_focus"] = json!(
                    "emulated only for explicit input targets; emulation disabled on disconnect"
                );
                state["foreground_launch_possible"] = json!(true);
                state["recommended_for"] = json!(
                    "Use browser tools for web pages; computer tools send desktop-wide input."
                );
                Ok(result(state))
            }
            "ssmcp.browser.launch" => {
                let args: Launch = parse(args)?;
                args.validate()?;
                let mut guard = self.session.lock().await;
                self.ensure_open()?;
                if guard.is_some() {
                    return Err("browser already connected; disconnect first".into());
                }
                let executable = installed_browser(args.executable.as_deref())?;
                let profile = create_profile()?;
                let profile_path = profile.0.clone();
                let mut command = tokio::process::Command::new(&executable);
                command
                    .current_dir(&self.root)
                    .arg(format!("--user-data-dir={}", profile_path.display()))
                    .arg("--remote-debugging-address=127.0.0.1")
                    .arg("--remote-debugging-port=0")
                    .arg("--no-first-run")
                    .arg("--no-default-browser-check")
                    .arg("--disable-background-networking")
                    // Match Playwright's Chromium startup: use a fresh surface
                    // instead of waiting on a redraw of an unpresented tab.
                    // Only our own temporary-profile browser gets this flag.
                    .arg("--enable-features=CDPScreenshotNewSurface")
                    .arg(format!("--window-size={},{}", args.width, args.height))
                    .arg("about:blank")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if args.headless {
                    command.arg("--headless=new");
                }
                let process = Managed::spawn(&mut command)
                    .map_err(|e| format!("could not start system browser: {e}"))?;
                // Ownership is established before the first cancellable await.
                let mut owned = OwnedBrowser {
                    process: Some(process),
                    profile: Some(profile),
                };
                let opened = tokio::time::timeout(Duration::from_secs(20), async {
                    loop {
                        if let Some(status) = owned.process.as_mut().unwrap().child.try_wait().map_err(|e|e.to_string())? {
                            return Err(format!("browser exited before CDP became ready ({status}); check the installed browser and desktop session"));
                        }
                        if let Ok(port_file) = tokio::fs::read_to_string(profile_path.join("DevToolsActivePort")).await
                            && let Some(port) = port_file.lines().next().and_then(|v|v.parse::<u16>().ok())
                            && port > 0
                        {
                            return self.attach(endpoint(&format!("http://127.0.0.1:{port}"))?).await;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }).await.map_err(|_|"system browser launch timed out".to_owned()).and_then(|v|v);
                match opened {
                    Ok(session) => {
                        let report = json!({"connected":true,"owned":true,"endpoint":session.endpoint.as_str(),
                            "executable":executable,"headless":args.headless,"browser":session.version});
                        *session.owned.lock().unwrap_or_else(|e| e.into_inner()) = Some(owned);
                        *guard = Some(Arc::new(session));
                        Ok(result(report))
                    }
                    Err(error) => {
                        owned.shutdown().await;
                        Err(error)
                    }
                }
            }
            "ssmcp.browser.connect" => {
                let args: Connect = parse(args)?;
                let endpoint = endpoint(&args.endpoint)?;
                let mut guard = self.session.lock().await;
                self.ensure_open()?;
                if guard.is_some() {
                    return Err("browser already connected; disconnect first".into());
                }
                let session = self.attach(endpoint).await?;
                let report = json!({"connected":true,"owned":false,"endpoint":session.endpoint.as_str(),"browser":session.version});
                *guard = Some(Arc::new(session));
                Ok(result(report))
            }
            "ssmcp.browser.disconnect" => {
                let _: Empty = parse(args)?;
                self.disconnect().await;
                Ok(result(json!({"connected":false})))
            }
            _ => self.connected(name, args).await,
        }
    }

    async fn attach(&self, endpoint: Url) -> Result<Session, String> {
        let version = self.get_json(&endpoint, "json/version").await?;
        let ws = websocket(
            &endpoint,
            version["webSocketDebuggerUrl"]
                .as_str()
                .ok_or("CDP endpoint has no browser WebSocket URL")?,
        )?;
        let browser = Cdp::connect(ws.as_str()).await?;
        let verified = browser.call("Browser.getVersion", json!({})).await?;
        Ok(Session {
            endpoint,
            browser,
            pages: StdMutex::new(HashMap::new()),
            owned: StdMutex::new(None),
            version: verified,
            closed: AtomicBool::new(false),
        })
    }

    async fn get_json(&self, endpoint: &Url, path: &str) -> Result<Value, String> {
        // Native browser tools remain available before any browser is attached.
        // Initialize HTTP/TLS machinery only when discovery actually needs it.
        let http = self
            .http
            .get_or_try_init(|| async {
                reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(3))
                    .timeout(CALL_TIMEOUT)
                    .build()
                    .map_err(|e| e.to_string())
            })
            .await?;
        let mut response = http
            .get(endpoint.join(path).map_err(|e| e.to_string())?)
            .send()
            .await
            .map_err(|e| format!("CDP discovery failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("CDP discovery failed: {e}"))?;
        if response
            .content_length()
            .is_some_and(|n| n > JSON_LIMIT as u64)
        {
            return Err("CDP discovery exceeds 1 MiB".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if bytes.len() + chunk.len() > JSON_LIMIT {
                return Err("CDP discovery exceeds 1 MiB".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid CDP discovery JSON: {e}"))
    }

    async fn targets(&self, session: &Session) -> Result<Vec<Value>, String> {
        self.ensure_session(session)?;
        // Only prune entries observed before this HTTP request. A concurrent
        // open may add a page after the response's target list was captured.
        let observed = session
            .pages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let targets = self.get_json(&session.endpoint, "json/list").await?;
        self.ensure_session(session)?;
        let targets = targets
            .as_array()
            .ok_or("CDP target list is not an array")?;
        let mut pages = session.pages.lock().unwrap_or_else(|e| e.into_inner());
        pages.retain(|id, entry| {
            let removed = observed.get(id).is_some_and(|old| Arc::ptr_eq(old, entry))
                && !targets.iter().any(|t| t["id"].as_str() == Some(id));
            if removed {
                entry.closed.store(true, Ordering::Release);
            }
            !removed
        });
        Ok(targets
            .iter()
            .filter(|t| t["type"] == "page")
            .cloned()
            .collect())
    }

    fn ensure_session(&self, session: &Session) -> Result<(), String> {
        self.ensure_open()?;
        if session.closed.load(Ordering::Acquire) {
            Err("browser session disconnected; select the current session again".into())
        } else {
            Ok(())
        }
    }

    async fn page(&self, session: &Session, id: &str) -> Result<OwnedMutexGuard<Page>, String> {
        valid_target(id)?;
        let entry = {
            let mut pages = session.pages.lock().unwrap_or_else(|e| e.into_inner());
            self.ensure_session(session)?;
            if !pages.contains_key(id) && pages.len() >= MAX_PAGES {
                return Err("64 active page connections reached; close a tab or reconnect".into());
            }
            pages
                .entry(id.to_owned())
                .or_insert_with(|| {
                    Arc::new(PageEntry {
                        connection: OnceCell::new(),
                        closed: AtomicBool::new(false),
                    })
                })
                .clone()
        };
        let connection = entry
            .connection
            .get_or_try_init(|| async {
                let mut initializing = InitializingPage {
                    session,
                    id,
                    entry: &entry,
                    armed: true,
                };
                self.ensure_session(session)?;
                if entry.closed.load(Ordering::Acquire) {
                    return Err("browser target was closed; call browser.tabs again".to_owned());
                }
                let targets = self.targets(session).await?;
                let target = targets
                    .iter()
                    .find(|t| t["id"].as_str() == Some(id))
                    .ok_or("target is not an open page; call browser.tabs again")?;
                let ws = websocket(
                    &session.endpoint,
                    target["webSocketDebuggerUrl"]
                        .as_str()
                        .ok_or("page has no CDP WebSocket URL")?,
                )?;
                let cdp = Cdp::connect(ws.as_str()).await?;
                initializing.armed = false;
                Ok(PageConnection {
                    cdp: cdp.clone(),
                    page: Arc::new(Mutex::new(Page {
                        cdp,
                        snapshot: None,
                    })),
                })
            })
            .await;
        let connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                // Failed or invalid targets must not consume the connection cap.
                let mut pages = session.pages.lock().unwrap_or_else(|e| e.into_inner());
                if pages
                    .get(id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    entry.closed.store(true, Ordering::Release);
                    pages.remove(id);
                }
                return Err(error);
            }
        };
        // Disconnect can race a connection's initialization. Close this late
        // socket explicitly; it was not necessarily visible to disconnect.
        if session.closed.load(Ordering::Acquire) || entry.closed.load(Ordering::Acquire) {
            connection.cdp.close().await;
            return Err("browser session or target disconnected".into());
        }
        let page = connection.page.clone().lock_owned().await;
        // A queued call belongs to the captured session, never its replacement.
        self.ensure_session(session)?;
        if entry.closed.load(Ordering::Acquire) {
            return Err("browser target was closed; call browser.tabs again".into());
        }
        Ok(page)
    }

    async fn connected(&self, name: &str, args: Value) -> Result<Value, String> {
        let session = self
            .session
            .lock()
            .await
            .clone()
            .ok_or("browser is not connected; use browser.launch or browser.connect")?;
        self.ensure_session(&session)?;
        let value = match name {
            "ssmcp.browser.tabs" => {
                let _: Empty = parse(args)?;
                let targets = self.targets(&session).await?;
                json!({"tabs":targets.iter().map(|t|json!({
                    "target_id":t["id"],"title":bounded(t["title"].as_str().unwrap_or(""),512),
                    "url":bounded(t["url"].as_str().unwrap_or(""),4096)
                })).collect::<Vec<_>>()})
            }
            "ssmcp.browser.open" => {
                let args: Open = parse(args)?;
                valid_url(&args.url)?;
                let opened = session
                    .browser
                    .call(
                        "Target.createTarget",
                        json!({"url":"about:blank","background":args.background}),
                    )
                    .await?;
                let id = opened["targetId"]
                    .as_str()
                    .ok_or("browser returned no target ID")?;
                let id = id.to_owned();
                let mut page = self.page(&session, &id).await?;
                navigate(&mut page.cdp, &args.url).await?;
                json!({"target_id":id,"url":args.url,"background":args.background,"input_scope":"browser_target","moves_system_pointer":false})
            }
            "ssmcp.browser.navigate" => {
                let args: Navigate = parse(args)?;
                valid_url(&args.url)?;
                let mut page = self.page(&session, &args.target_id).await?;
                page.snapshot = None;
                let navigation = navigate(&mut page.cdp, &args.url).await?;
                json!({"target_id":args.target_id,"navigation":navigation})
            }
            "ssmcp.browser.snapshot" => {
                let args: SnapshotArgs = parse(args)?;
                if !(1..=200).contains(&args.max_elements) || !(1..=32768).contains(&args.max_chars)
                {
                    return Err("snapshot bounds: max_elements 1..200, max_chars 1..32768".into());
                }
                let token = format!("s{}", self.next_snapshot.fetch_add(1, Ordering::Relaxed));
                let mut page = self.page(&session, &args.target_id).await?;
                // Release the old remote object before publishing a new set of refs.
                if let Some(old) = page.snapshot.take() {
                    let _ = page
                        .cdp
                        .call("Runtime.releaseObject", json!({"objectId":old.object_id}))
                        .await;
                }
                let script = SNAPSHOT_SCRIPT
                    .replace("$MAX_ELEMENTS", &args.max_elements.to_string())
                    .replace("$MAX_CHARS", &args.max_chars.to_string())
                    .replace("$TOKEN", &json!(token).to_string());
                let reply = page.cdp.call("Runtime.evaluate",json!({
                    "expression":script,"returnByValue":false,"objectGroup":"ssmcp-snapshot","timeout":10000
                })).await?;
                if let Some(exception) = reply.get("exceptionDetails") {
                    return Err(format!("snapshot JavaScript exception: {exception}"));
                }
                let object_id = reply["result"]["objectId"]
                    .as_str()
                    .ok_or("snapshot has no DOM object")?
                    .to_owned();
                let info = page.cdp.call("Runtime.callFunctionOn",json!({
                    "objectId":object_id,"functionDeclaration":"function(){ return this.info; }",
                    "returnByValue":true
                })).await?;
                let mut info = remote_value_bounded(info, RPC_LIMIT)?;
                info["target_id"] = json!(args.target_id);
                let info = bound_snapshot(info);
                let count = info["elements"].as_array().map_or(0, Vec::len);
                page.snapshot = Some(Snapshot {
                    token,
                    object_id,
                    count,
                });
                info
            }
            "ssmcp.browser.click" => {
                let args: Click = parse(args)?;
                if !["left", "right", "middle"].contains(&args.button.as_str())
                    || !(1..=3).contains(&args.click_count)
                {
                    return Err("invalid mouse button or click_count".into());
                }
                let mut page = self.page(&session, &args.target_id).await?;
                page.cdp.prepare_input().await?;
                let point = ref_action(&mut page, &args.element_ref, CLICK_SCRIPT, vec![]).await?;
                let x = point["x"].as_f64().ok_or("element has no x coordinate")?;
                let y = point["y"].as_f64().ok_or("element has no y coordinate")?;
                let button_code = match args.button.as_str() {
                    "left" => 1,
                    "right" => 2,
                    _ => 4,
                };
                page.cdp
                    .call(
                        "Input.dispatchMouseEvent",
                        json!({"type":"mouseMoved","x":x,"y":y}),
                    )
                    .await?;
                page.cdp.input_pair("Input.dispatchMouseEvent",
                    json!({"type":"mousePressed","x":x,"y":y,"button":args.button,"buttons":button_code,"clickCount":args.click_count}),
                    json!({"type":"mouseReleased","x":x,"y":y,"button":args.button,"buttons":0,"clickCount":args.click_count})
                ).await?;
                json!({"target_id":args.target_id,"ref":args.element_ref,"clicked":true})
            }
            "ssmcp.browser.fill" => {
                let args: Fill = parse(args)?;
                if args.text.len() > 32768 {
                    return Err("fill text exceeds 32768 UTF-8 bytes".into());
                }
                let mut page = self.page(&session, &args.target_id).await?;
                page.cdp.prepare_input().await?;
                let prepared = ref_action(
                    &mut page,
                    &args.element_ref,
                    FILL_SCRIPT,
                    vec![json!(args.text)],
                )
                .await?;
                if prepared["insert_text"] == true {
                    page.cdp
                        .call("Input.insertText", json!({"text":args.text}))
                        .await?;
                }
                json!({"target_id":args.target_id,"ref":args.element_ref,"filled":true})
            }
            "ssmcp.browser.key" => {
                let args: Key = parse(args)?;
                let (code, key, text) = key_spec(&args.key)?;
                let modifiers = modifier_bits(&args.modifiers)?;
                let page = self.page(&session, &args.target_id).await?;
                page.cdp.prepare_input().await?;
                let mut down = json!({"type":"keyDown","key":key,"windowsVirtualKeyCode":code,
                    "nativeVirtualKeyCode":code,"modifiers":modifiers});
                if modifiers & (1 | 2 | 4) == 0
                    && let Some(text) = text
                {
                    down["text"] = json!(text);
                }
                page.cdp
                    .input_pair(
                        "Input.dispatchKeyEvent",
                        down,
                        json!({"type":"keyUp","key":key,"windowsVirtualKeyCode":code,
                        "nativeVirtualKeyCode":code,"modifiers":modifiers}),
                    )
                    .await?;
                json!({"target_id":args.target_id,"key":args.key,"pressed":true})
            }
            "ssmcp.browser.scroll" => {
                let args: Scroll = parse(args)?;
                if !(-100000..=100000).contains(&args.delta_x)
                    || !(-100000..=100000).contains(&args.delta_y)
                {
                    return Err("scroll delta exceeds 100000 CSS pixels".into());
                }
                let page = self.page(&session, &args.target_id).await?;
                page.cdp.prepare_input().await?;
                let size = page
                    .cdp
                    .value("({width:innerWidth,height:innerHeight})")
                    .await?;
                page.cdp
                    .call(
                        "Input.dispatchMouseEvent",
                        json!({"type":"mouseWheel",
                    "x":size["width"].as_f64().unwrap_or(800.0)/2.0,
                    "y":size["height"].as_f64().unwrap_or(600.0)/2.0,
                    "deltaX":args.delta_x,"deltaY":args.delta_y}),
                    )
                    .await?;
                json!({"target_id":args.target_id,"scrolled":true})
            }
            "ssmcp.browser.wait_for" => {
                let args: WaitFor = parse(args)?;
                args.validate()?;
                let started = tokio::time::Instant::now();
                // JSON encoding keeps requested text data, never page code.
                let expression = format!(
                    "(document.body ? document.body.innerText : '').includes({})",
                    json!(args.text)
                );
                tokio::time::timeout(Duration::from_millis(args.timeout_ms), async {
                    let page = self.page(&session, &args.target_id).await?;
                    loop {
                        self.ensure_session(&session)?;
                        let present = page
                            .cdp
                            .value(&expression)
                            .await?
                            .as_bool()
                            .ok_or("browser text check returned no boolean")?;
                        if present == (args.state == "present") {
                            return Ok::<(), String>(());
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await
                .map_err(|_| {
                    format!(
                        "browser.wait_for timed out after {} ms waiting for text to be {}",
                        args.timeout_ms, args.state
                    )
                })??;
                json!({"target_id":args.target_id,"matched":true,"state":args.state,
                    "elapsed_ms":started.elapsed().as_millis()})
            }
            "ssmcp.browser.evaluate" => {
                let args: Evaluate = parse(args)?;
                if args.expression.len() > 65536 {
                    return Err("expression exceeds 65536 UTF-8 bytes".into());
                }
                let page = self.page(&session, &args.target_id).await?;
                let value = page.cdp.value(&args.expression).await?;
                json!({"target_id":args.target_id,"value":value})
            }
            "ssmcp.browser.screenshot" => {
                let args: Screenshot = parse(args)?;
                if !(320..=2560).contains(&args.max_width) {
                    return Err("max_width must be 320..2560".into());
                }
                let mut page = self.page(&session, &args.target_id).await?;
                return screenshot(&mut page.cdp, &args.target_id, args.max_width).await;
            }
            "ssmcp.browser.close" => {
                let args: Target = parse(args)?;
                valid_target(&args.target_id)?;
                let _page = self.page(&session, &args.target_id).await?;
                let targets = self.targets(&session).await?;
                if !targets.iter().any(|t| t["id"] == args.target_id) {
                    return Err("target is not an open page".into());
                }
                let closed = session
                    .browser
                    .call("Target.closeTarget", json!({"targetId":args.target_id}))
                    .await?;
                if closed["success"] == true
                    && let Some(entry) = session
                        .pages
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&args.target_id)
                {
                    entry.closed.store(true, Ordering::Release);
                }
                json!({"target_id":args.target_id,"closed":closed["success"]})
            }
            _ => return Err(format!("unknown browser tool: {name}")),
        };
        Ok(result(value))
    }

    async fn disconnect(&self) {
        let session = {
            let mut guard = self.session.lock().await;
            let session = guard.take();
            if let Some(session) = &session {
                session.closed.store(true, Ordering::Release);
            }
            session
        };
        if let Some(session) = session {
            let entries =
                std::mem::take(&mut *session.pages.lock().unwrap_or_else(|e| e.into_inner()));
            let mut connections = Vec::with_capacity(entries.len() + 1);
            for entry in entries.values() {
                entry.closed.store(true, Ordering::Release);
                if let Some(connection) = entry.connection.get() {
                    connections.push(connection.cdp.clone());
                }
            }
            connections.push(session.browser.clone());
            // Existing bounded input release and connection drain remain intact.
            // New calls may attach elsewhere without entering this old session.
            futures_util::future::join_all(connections.iter().map(Cdp::close)).await;
            let owned = session
                .owned
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(mut owned) = owned {
                owned.shutdown().await;
            }
        }
    }

    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        self.disconnect().await;
    }
}

async fn navigate(cdp: &mut Cdp, url: &str) -> Result<Value, String> {
    let navigation = cdp.call("Page.navigate", json!({"url":url})).await?;
    if let Some(error) = navigation["errorText"].as_str() {
        return Err(format!("browser navigation failed: {error}"));
    }
    if navigation["isDownload"] == true {
        return Err("navigation started a download instead of an interactive document".into());
    }
    let loader = navigation["loaderId"].as_str();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let tree = cdp.call("Page.getFrameTree", json!({})).await?;
        let committed =
            loader.is_none_or(|id| tree["frameTree"]["frame"]["loaderId"].as_str() == Some(id));
        if committed {
            match cdp.value("document.readyState").await {
                Ok(value) if value == "interactive" || value == "complete" => {
                    return Ok(navigation);
                }
                Err(error) if !error.contains("context") && !error.contains("navigation") => {
                    return Err(error);
                }
                _ => {}
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(
                "navigation did not produce an interactive document within 15 seconds".into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

const SNAPSHOT_INLINE_LIMIT: usize = 24 * 1024;
fn bound_snapshot(mut info: Value) -> Value {
    if shrink_field(&mut info, "text", usize::MAX) {
        info["text_truncated"] = json!(true);
    }
    for (key, limit) in [("url", 2048), ("title", 1024)] {
        if shrink_field(&mut info, key, limit) {
            info[format!("{key}_truncated")] = json!(true);
        }
    }
    let mut truncated = false;
    if let Some(elements) = info["elements"].as_array_mut() {
        for element in elements {
            for (key, limit) in [("name", 512), ("value", 256), ("role", 128), ("type", 64)] {
                truncated |= shrink_field(element, key, limit);
            }
        }
    }
    info["metadata_truncated"] = json!(truncated);
    info["inline_budget_bytes"] = json!(SNAPSHOT_INLINE_LIMIT);
    // Count the entire MCP result, including text/structured JSON duplication.
    while result(info.clone()).to_string().len() > SNAPSHOT_INLINE_LIMIT {
        let bytes = info["text"].as_str().map_or(0, str::len);
        if bytes > 512 {
            shrink_field(&mut info, "text", bytes / 2);
            info["text_truncated"] = json!(true);
        } else if let Some(elements) = info["elements"].as_array_mut()
            && !elements.is_empty()
        {
            elements.truncate(elements.len() / 2);
            info["elements_truncated"] = json!(true);
        } else if bytes > 0 {
            shrink_field(&mut info, "text", bytes / 2);
            info["text_truncated"] = json!(true);
        } else {
            break;
        }
    }
    info
}
fn shrink_field(value: &mut Value, key: &str, limit: usize) -> bool {
    let Some(text) = value[key].as_str() else {
        return false;
    };
    let mut end = text.len().min(limit);
    let mut lines = 0;
    for (index, byte) in text.bytes().take(end).enumerate() {
        if byte == b'\n' {
            lines += 1;
            if lines == 400 {
                end = index + 1;
                break;
            }
        }
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == text.len() {
        return false;
    }
    let shortened = text[..end].to_owned();
    value[key] = json!(shortened);
    true
}

async fn ref_action(
    page: &mut Page,
    element_ref: &str,
    script: &str,
    extra: Vec<Value>,
) -> Result<Value, String> {
    let snapshot = page
        .snapshot
        .as_ref()
        .ok_or("take browser.snapshot before using element refs")?;
    let prefix = format!("{}e", snapshot.token);
    let index = element_ref
        .strip_prefix(&prefix)
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n >= 1 && *n <= snapshot.count)
        .ok_or("ref is not from this target's latest snapshot")?
        - 1;
    let mut arguments = vec![json!({"value":index})];
    arguments.extend(extra.into_iter().map(|value| json!({"value":value})));
    let function = REF_SCRIPT.replace("$ACTION", script);
    let reply = page
        .cdp
        .call(
            "Runtime.callFunctionOn",
            json!({
                "objectId":snapshot.object_id,"functionDeclaration":function,"arguments":arguments,
                "returnByValue":true,"awaitPromise":true
            }),
        )
        .await
        .map_err(|e| format!("{e}; refresh browser.snapshot if the page changed"))?;
    remote_value(reply)
}

async fn screenshot(cdp: &mut Cdp, target_id: &str, max_width: u32) -> Result<Value, String> {
    let metrics = cdp.call("Page.getLayoutMetrics", json!({})).await?;
    let viewport = metrics
        .get("cssVisualViewport")
        .or_else(|| metrics.get("visualViewport"))
        .ok_or("browser returned no viewport metrics")?;
    let width = viewport["clientWidth"]
        .as_f64()
        .filter(|n| n.is_finite() && *n > 0.0 && *n <= 16384.0)
        .ok_or("invalid browser viewport width")?;
    let height = viewport["clientHeight"]
        .as_f64()
        .filter(|n| n.is_finite() && *n > 0.0 && *n <= 16384.0)
        .ok_or("invalid browser viewport height")?;
    let x = viewport["pageX"].as_f64().unwrap_or(0.0);
    let y = viewport["pageY"].as_f64().unwrap_or(0.0);
    let mut scale = (max_width as f64 / width).min(1.0);
    for quality in [70, 50, 35] {
        let reply=cdp.call("Page.captureScreenshot",json!({
            "format":"jpeg","quality":quality,"fromSurface":true,"captureBeyondViewport":false,
            "clip":{"x":x,"y":y,"width":width,"height":height,"scale":scale}
        })).await.map_err(|error| {
            if error.contains("Page.captureScreenshot timed out") {
                format!(
                    "{error}; {}",
                    concat!(
                        "This browser did not produce the target's surface within 15 seconds. ",
                        "The tab was not activated. Use browser.snapshot for page structure, ",
                        "an isolated headless browser or private desktop for visual capture, ",
                        "or explicitly choose foreground interaction."
                    )
                )
            } else {
                error
            }
        })?;
        let data = reply["data"]
            .as_str()
            .ok_or("browser returned no screenshot data")?;
        if data.len() <= IMAGE_LIMIT.div_ceil(3) * 4 {
            let bytes = STANDARD
                .decode(data)
                .map_err(|e| format!("invalid screenshot base64: {e}"))?;
            if bytes.len() <= IMAGE_LIMIT {
                let (pixel_width, pixel_height) =
                    jpeg_dimensions(&bytes).ok_or("browser returned invalid JPEG")?;
                let metadata = json!({"target_id":target_id,"mime_type":"image/jpeg",
                    "width":pixel_width,"height":pixel_height,"scale":scale,"bytes":bytes.len()});
                return Ok(
                    json!({"content":[{"type":"image","mimeType":"image/jpeg","data":data},
                    {"type":"text","text":metadata.to_string()}],"structuredContent":metadata,"isError":false}),
                );
            }
        }
        scale *= 0.75;
    }
    Err("screenshot exceeds the 400 KiB image limit; reduce max_width".into())
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u16, u16)> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut p = 2;
    while p + 4 <= bytes.len() {
        if bytes[p] != 0xff {
            return None;
        }
        while p < bytes.len() && bytes[p] == 0xff {
            p += 1;
        }
        let marker = *bytes.get(p)?;
        p += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length = u16::from_be_bytes([*bytes.get(p)?, *bytes.get(p + 1)?]) as usize;
        if length < 2 || p + length > bytes.len() {
            return None;
        }
        if [
            0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf,
        ]
        .contains(&marker)
        {
            if length < 7 {
                return None;
            }
            let height = u16::from_be_bytes([bytes[p + 3], bytes[p + 4]]);
            let width = u16::from_be_bytes([bytes[p + 5], bytes[p + 6]]);
            return (width > 0 && height > 0).then_some((width, height));
        }
        p += length;
    }
    None
}

fn endpoint(text: &str) -> Result<Url, String> {
    if text.len() > 2048 {
        return Err("browser endpoint is too long".into());
    }
    let mut url = Url::parse(text).map_err(|e| format!("invalid browser endpoint: {e}"))?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || !loopback(&url)
    {
        return Err(
            "browser endpoint must be a literal loopback HTTP origin, e.g. http://127.0.0.1:9222"
                .into(),
        );
    }
    if url.port_or_known_default() == Some(0) {
        return Err("browser endpoint port must be nonzero".into());
    }
    url.set_path("/");
    Ok(url)
}

fn loopback(url: &Url) -> bool {
    url.host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback())
}

fn websocket(endpoint: &Url, text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|e| format!("invalid CDP WebSocket: {e}"))?;
    if url.scheme() != "ws"
        || !loopback(&url)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str() != endpoint.host_str()
        || url.port_or_known_default() != endpoint.port_or_known_default()
        || !url.path().starts_with("/devtools/")
    {
        return Err("CDP WebSocket must stay on the explicitly selected loopback endpoint".into());
    }
    Ok(url)
}

fn valid_target(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err("invalid browser target_id".into());
    }
    Ok(())
}

fn valid_url(text: &str) -> Result<(), String> {
    if text.len() > 32768 {
        return Err("page URL exceeds 32768 bytes".into());
    }
    let url = Url::parse(text).map_err(|e| format!("invalid page URL: {e}"))?;
    if !["http", "https", "file", "data"].contains(&url.scheme()) && text != "about:blank" {
        return Err("supported page URL schemes: http, https, file, data, or about:blank".into());
    }
    Ok(())
}

fn installed_browser(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(explicit) = explicit {
        if explicit.len() > 4096 {
            return Err("browser executable path is too long".into());
        }
        let path = PathBuf::from(explicit);
        if path.is_absolute() {
            return if crate::platform::is_executable(&path) {
                Ok(path)
            } else {
                Err("browser executable is unavailable".into())
            };
        }
        return crate::program::executable(explicit);
    }
    for name in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "msedge",
    ] {
        if let Ok(path) = crate::program::executable(name) {
            return Ok(path);
        }
    }
    #[cfg(target_os = "macos")]
    for path in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ] {
        if crate::platform::is_executable(std::path::Path::new(path)) {
            return Ok(path.into());
        }
    }
    #[cfg(windows)]
    for base in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(base) {
            for tail in [
                r"Google\Chrome\Application\chrome.exe",
                r"Microsoft\Edge\Application\msedge.exe",
                r"Chromium\Application\chrome.exe",
            ] {
                let path = PathBuf::from(&base).join(tail);
                if crate::platform::is_executable(&path) {
                    return Ok(path);
                }
            }
        }
    }
    Err("no system Chromium browser found; install Chromium/Chrome/Edge or pass executable".into())
}

fn create_profile() -> Result<Profile, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let path = std::env::temp_dir().join(format!("ssmcp-browser-{suffix}"));
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    builder
        .create(&path)
        .map_err(|e| format!("browser profile: {e}"))?;
    Ok(Profile(path))
}

fn bounded(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid browser arguments: {e}"))
}
fn merge(mut base: Value, extra: Value) -> Value {
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}
fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({"name":format!("ssmcp.browser.{name}"),"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only}})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    endpoint: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    target_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    url: String,
    #[serde(default = "default_true")]
    background: bool,
}
fn default_true() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitFor {
    target_id: String,
    text: String,
    #[serde(default = "default_wait_state")]
    state: String,
    #[serde(default = "default_wait_timeout")]
    timeout_ms: u64,
}
fn default_wait_state() -> String {
    "present".into()
}
fn default_wait_timeout() -> u64 {
    5000
}
impl WaitFor {
    fn validate(&self) -> Result<(), String> {
        if self.text.is_empty() || self.text.len() > 1024 {
            return Err("wait_for text must contain 1..1024 UTF-8 bytes".into());
        }
        if !matches!(self.state.as_str(), "present" | "absent")
            || !(1..=30000).contains(&self.timeout_ms)
        {
            return Err("wait_for requires state present/absent and timeout_ms 1..30000".into());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Navigate {
    target_id: String,
    url: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evaluate {
    target_id: String,
    expression: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    executable: Option<String>,
    #[serde(default = "default_true")]
    headless: bool,
    #[serde(default = "default_width")]
    width: u32,
    #[serde(default = "default_height")]
    height: u32,
}
fn default_width() -> u32 {
    1280
}
fn default_height() -> u32 {
    800
}
impl Launch {
    fn validate(&self) -> Result<(), String> {
        if !(320..=4096).contains(&self.width) || !(240..=4096).contains(&self.height) {
            return Err("browser width must be 320..4096 and height 240..4096".into());
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotArgs {
    target_id: String,
    #[serde(default = "default_elements")]
    max_elements: usize,
    #[serde(default = "default_chars")]
    max_chars: usize,
}
fn default_elements() -> usize {
    100
}
fn default_chars() -> usize {
    12000
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Click {
    target_id: String,
    #[serde(rename = "ref")]
    element_ref: String,
    #[serde(default = "left_button")]
    button: String,
    #[serde(default = "one")]
    click_count: u32,
}
fn left_button() -> String {
    "left".into()
}
fn one() -> u32 {
    1
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fill {
    target_id: String,
    #[serde(rename = "ref")]
    element_ref: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    target_id: String,
    key: String,
    #[serde(default)]
    modifiers: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scroll {
    target_id: String,
    #[serde(default)]
    delta_x: i64,
    #[serde(default)]
    delta_y: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Screenshot {
    target_id: String,
    #[serde(default = "default_width")]
    max_width: u32,
}
fn modifier_bits(modifiers: &[String]) -> Result<u8, String> {
    if modifiers.len() > 4 {
        return Err("at most four key modifiers are supported".into());
    }
    let mut bits = 0;
    for modifier in modifiers {
        let bit = match modifier.as_str() {
            "Alt" => 1,
            "Control" => 2,
            "Meta" => 4,
            "Shift" => 8,
            _ => return Err("unknown key modifier".into()),
        };
        if bits & bit != 0 {
            return Err("duplicate key modifier".into());
        }
        bits |= bit;
    }
    Ok(bits)
}
fn key_spec(key: &str) -> Result<(u32, String, Option<String>), String> {
    let code = match key {
        "Backspace" => 8,
        "Tab" => 9,
        "Enter" => 13,
        "Escape" => 27,
        "Space" => 32,
        "PageUp" => 33,
        "PageDown" => 34,
        "End" => 35,
        "Home" => 36,
        "ArrowLeft" => 37,
        "ArrowUp" => 38,
        "ArrowRight" => 39,
        "ArrowDown" => 40,
        "Insert" => 45,
        "Delete" => 46,
        _ => {
            let mut chars = key.chars();
            let ch = chars.next().ok_or("key is empty")?;
            if chars.next().is_some() || ch.is_control() {
                return Err("unsupported browser key".into());
            }
            let code = if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase() as u32
            } else {
                0
            };
            return Ok((code, key.into(), Some(key.into())));
        }
    };
    let actual = if key == "Space" { " " } else { key };
    let text = match key {
        "Enter" => Some("\r".into()),
        "Space" => Some(" ".into()),
        _ => None,
    };
    Ok((code, actual.into(), text))
}

const SNAPSHOT_SCRIPT: &str = r#"(() => {
    const token=$TOKEN, limit=$MAX_ELEMENTS, maxChars=$MAX_CHARS;
    const visible = e => {
        const r=e.getBoundingClientRect(), s=getComputedStyle(e);
        return r.width>0 && r.height>0 && s.visibility!=='hidden' && s.display!=='none';
    };
    const candidates=Array.from(document.querySelectorAll('a,button,input,textarea,select,summary,[role],[contenteditable="true"],h1,h2,h3,h4,h5,h6,img[alt]')).filter(visible);
    const nodes=candidates.slice(0,limit);
    const bodyText=document.body ? document.body.innerText : '';
    const text=bodyText.slice(0,maxChars);
    const implicitRole=e=>{
        const tag=e.tagName.toLowerCase();
        if(tag==='input') return ({checkbox:'checkbox',radio:'radio',button:'button',submit:'button',reset:'button',image:'button',range:'slider',number:'spinbutton',search:'searchbox'})[e.type] ?? (['text','email','url','tel','password'].includes(e.type) ? 'textbox' : '');
        if(tag==='select') return e.multiple || e.size>1 ? 'listbox' : 'combobox';
        if(tag==='a') return e.hasAttribute('href') ? 'link' : '';
        if(/^h[1-6]$/.test(tag)) return 'heading';
        return ({button:'button',textarea:'textbox',summary:'button',img:'img'})[tag] || (e.isContentEditable ? 'textbox' : '');
    };
    const items=nodes.map((e,i)=>{
        const r=e.getBoundingClientRect();
        const labelled=(e.getAttribute('aria-labelledby')||'').trim().split(/\s+/).slice(0,32)
            .map(id=>document.getElementById(id)?.textContent||'').join(' ').trim();
        const label=labelled || e.getAttribute('aria-label') || (e.labels ? Array.from(e.labels).map(x=>x.innerText).join(' ') : '') ||
            e.getAttribute('alt') || e.getAttribute('placeholder') || e.innerText || e.getAttribute('title') || '';
        return {ref:token+'e'+(i+1),tag:e.tagName.toLowerCase(),role:(e.getAttribute('role')||implicitRole(e)).slice(0,80),
            name:label.slice(0,400),type:(e.getAttribute('type')||'').slice(0,80),
            value:e.type==='password'?undefined:typeof e.value==='string'?e.value.slice(0,1000):undefined,
            disabled:!!e.disabled,checked:typeof e.checked==='boolean'?e.checked:undefined,
            x:Math.round(r.x),y:Math.round(r.y),width:Math.round(r.width),height:Math.round(r.height)};
    });
    return {doc:document,href:location.href,nodes,info:{snapshot_id:token,url:location.href,
        title:document.title,text,elements:items,elements_truncated:candidates.length>limit,
        text_truncated:bodyText.length>maxChars,
        viewport:{width:innerWidth,height:innerHeight},scope:'main document; iframe contents omitted'}};
})()"#;
const REF_SCRIPT: &str = r#"function(index,value){
    if(this.doc!==document || this.href!==location.href) throw Error('stale snapshot; take a new browser.snapshot');
    const e=this.nodes[index];
    if(!e || !e.isConnected || e.ownerDocument!==document) throw Error('detached element; take a new browser.snapshot');
    if(e.disabled) throw Error('element is disabled');
    $ACTION
}"#;
const CLICK_SCRIPT: &str = r#"
    e.scrollIntoView({block:'center',inline:'center',behavior:'instant'});
    const r=e.getBoundingClientRect();
    if(r.width<=0 || r.height<=0) throw Error('element is not visible');
    const x=Math.max(0,Math.min(innerWidth-1,r.x+r.width/2));
    const y=Math.max(0,Math.min(innerHeight-1,r.y+r.height/2));
    const hit=document.elementFromPoint(x,y);
    if(hit!==e && !e.contains(hit)) throw Error('element is obscured at its click point');
    return {x,y};
"#;
const FILL_SCRIPT: &str = r#"
    e.scrollIntoView({block:'center',inline:'center',behavior:'instant'});
    if(e.readOnly) throw Error('element is read-only');
    if(e.tagName==='SELECT'){
        if(!Array.from(e.options).some(o=>o.value===value)) throw Error('select option does not exist');
        e.value=value;e.dispatchEvent(new Event('input',{bubbles:true}));e.dispatchEvent(new Event('change',{bubbles:true}));
        return {insert_text:false};
    }
    e.focus();
    if(e.tagName==='INPUT' || e.tagName==='TEXTAREA'){
        if(e.tagName==='INPUT' && ['button','submit','reset','file','checkbox','radio','range','color','hidden'].includes(e.type))
            throw Error('element does not accept text');
        e.select();
    } else if(e.isContentEditable){
        const range=document.createRange();range.selectNodeContents(e);
        const selection=getSelection();selection.removeAllRanges();selection.addRange(range);
    } else { throw Error('element is not editable'); }
    return {insert_text:true};
"#;

#[cfg(test)]
mod tests {
    use super::*;

    async fn poll_once<F: std::future::Future>(
        mut future: std::pin::Pin<&mut F>,
    ) -> std::task::Poll<F::Output> {
        std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(future.as_mut(), cx))
        })
        .await
    }

    async fn test_cdp() -> (Cdp, WebSocketStream<TcpStream>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("ws://{address}/devtools/page/test");
        let (client, server) = tokio::join!(Cdp::connect(&url), async {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });
        (client.unwrap(), server)
    }

    fn test_session(pages: Vec<(&str, Cdp)>) -> Arc<Session> {
        let browser = pages[0].1.clone();
        let mut entries = HashMap::new();
        for (id, cdp) in pages {
            let connection = OnceCell::new();
            assert!(
                connection
                    .set(PageConnection {
                        cdp: cdp.clone(),
                        page: Arc::new(Mutex::new(Page {
                            cdp,
                            snapshot: None
                        })),
                    })
                    .is_ok()
            );
            entries.insert(
                id.to_owned(),
                Arc::new(PageEntry {
                    connection,
                    closed: AtomicBool::new(false),
                }),
            );
        }
        Arc::new(Session {
            endpoint: endpoint("http://127.0.0.1:9").unwrap(),
            browser,
            pages: StdMutex::new(entries),
            owned: StdMutex::new(None),
            version: json!({}),
            closed: AtomicBool::new(false),
        })
    }

    async fn test_browser(pages: Vec<(&str, Cdp)>) -> Arc<Browser> {
        let browser = Arc::new(Browser::new(std::env::current_dir().unwrap()).unwrap());
        *browser.session.lock().await = Some(test_session(pages));
        browser
    }

    async fn test_request(socket: &mut WebSocketStream<TcpStream>) -> Value {
        serde_json::from_str(&socket.next().await.unwrap().unwrap().into_text().unwrap()).unwrap()
    }

    async fn test_reply(socket: &mut WebSocketStream<TcpStream>, request: &Value, value: Value) {
        socket
            .send(Message::Text(
                json!({"id":request["id"],"result":value})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn opening_tabs_defaults_to_background_and_allows_explicit_foreground() {
        for background in [None, Some(false)] {
            let (cdp, mut socket) = test_cdp().await;
            let browser = test_browser(vec![("a", cdp)]).await;
            let expected = background.unwrap_or(true);
            let server = tokio::spawn(async move {
                let request = test_request(&mut socket).await;
                assert_eq!(request["method"], "Target.createTarget");
                assert_eq!(request["params"]["background"], expected);
                test_reply(&mut socket, &request, json!({"targetId":"a"})).await;
                let request = test_request(&mut socket).await;
                assert_eq!(request["method"], "Page.navigate");
                test_reply(
                    &mut socket,
                    &request,
                    json!({"frameId":"main","loaderId":"l"}),
                )
                .await;
                let request = test_request(&mut socket).await;
                assert_eq!(request["method"], "Page.getFrameTree");
                test_reply(
                    &mut socket,
                    &request,
                    json!({"frameTree":{"frame":{"loaderId":"l"}}}),
                )
                .await;
                let request = test_request(&mut socket).await;
                assert_eq!(request["params"]["expression"], "document.readyState");
                test_reply(
                    &mut socket,
                    &request,
                    json!({"result":{"value":"complete"}}),
                )
                .await;
            });
            let mut args = json!({"url":"https://example.test/"});
            if let Some(background) = background {
                args["background"] = json!(background);
            }
            let reply = browser.perform("ssmcp.browser.open", args).await.unwrap();
            assert_eq!(reply["structuredContent"]["background"], expected);
            assert_eq!(reply["structuredContent"]["moves_system_pointer"], false);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn wait_for_observes_text_changes_locally_and_times_out_without_input() {
        let (cdp, mut socket) = test_cdp().await;
        let browser = test_browser(vec![("a", cdp)]).await;
        let server = tokio::spawn(async move {
            for present in [false, true, false] {
                let request = test_request(&mut socket).await;
                assert_eq!(request["method"], "Runtime.evaluate");
                let expression = request["params"]["expression"].as_str().unwrap();
                assert!(expression.contains(".includes("));
                test_reply(&mut socket, &request, json!({"result":{"value":present}})).await;
            }
            let request = test_request(&mut socket).await;
            assert_eq!(request["method"], "Runtime.evaluate");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let appeared = browser
            .perform(
                "ssmcp.browser.wait_for",
                json!({"target_id":"a","text":"Ready","timeout_ms":1000}),
            )
            .await
            .unwrap();
        assert_eq!(appeared["structuredContent"]["matched"], true);
        let absent = browser
            .perform(
                "ssmcp.browser.wait_for",
                json!({"target_id":"a","text":"Ready","state":"absent","timeout_ms":1000}),
            )
            .await
            .unwrap();
        assert_eq!(absent["structuredContent"]["matched"], true);
        let started = tokio::time::Instant::now();
        let error = browser
            .perform(
                "ssmcp.browser.wait_for",
                json!({"target_id":"a","text":"Ready","timeout_ms":20}),
            )
            .await
            .unwrap_err();
        assert!(error.contains("timed out after 20 ms"));
        assert!(started.elapsed() < Duration::from_secs(1));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn different_pages_progress_while_one_page_preserves_action_order() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (cdp_a, mut socket_a) = test_cdp().await;
            let (cdp_b, mut socket_b) = test_cdp().await;
            let browser = test_browser(vec![("a", cdp_a), ("b", cdp_b)]).await;
            let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let server_a = tokio::spawn(async move {
                let first = test_request(&mut socket_a).await;
                assert_eq!(first["method"], "Runtime.evaluate");
                observed_tx.send(()).unwrap();
                release_rx.await.unwrap();
                test_reply(
                    &mut socket_a,
                    &first,
                    json!({"result":{"type":"object","objectId":"snapshot-a"}}),
                )
                .await;
                // A same-page call queued during the first RPC must not run
                // between the two RPCs that publish one snapshot.
                let second = test_request(&mut socket_a).await;
                assert_eq!(second["method"], "Runtime.callFunctionOn");
                test_reply(
                    &mut socket_a,
                    &second,
                    json!({"result":{"value":{
                        "snapshot_id":"s1","title":"A","url":"https://example.test/",
                        "text":"A","elements":[]
                    }}}),
                )
                .await;
                let third = test_request(&mut socket_a).await;
                assert_eq!(third["params"]["expression"], "'same-page'");
                test_reply(
                    &mut socket_a,
                    &third,
                    json!({"result":{"value":"same-page"}}),
                )
                .await;
            });
            let server_b = tokio::spawn(async move {
                let request = test_request(&mut socket_b).await;
                assert_eq!(request["params"]["expression"], "'other-page'");
                test_reply(
                    &mut socket_b,
                    &request,
                    json!({"result":{"value":"other-page"}}),
                )
                .await;
            });
            let snapshot = tokio::spawn({
                let browser = browser.clone();
                async move {
                    browser
                        .perform("ssmcp.browser.snapshot", json!({"target_id":"a"}))
                        .await
                }
            });
            observed_rx.await.unwrap();
            let same_page = browser.perform(
                "ssmcp.browser.evaluate",
                json!({"target_id":"a","expression":"'same-page'"}),
            );
            tokio::pin!(same_page);
            assert!(poll_once(same_page.as_mut()).await.is_pending());
            let other = browser
                .perform(
                    "ssmcp.browser.evaluate",
                    json!({"target_id":"b","expression":"'other-page'"}),
                )
                .await
                .unwrap();
            assert_eq!(other["structuredContent"]["value"], "other-page");
            // Page B completed while A's native response was still withheld.
            assert!(!snapshot.is_finished());
            release_tx.send(()).unwrap();
            assert_eq!(
                snapshot.await.unwrap().unwrap()["structuredContent"]["target_id"],
                "a"
            );
            assert_eq!(
                same_page.await.unwrap()["structuredContent"]["value"],
                "same-page"
            );
            server_a.await.unwrap();
            server_b.await.unwrap();
            browser.disconnect().await;
        })
        .await
        .expect("independent pages were serialized or a fixture stalled");
    }

    #[tokio::test]
    async fn cancelled_page_initialization_does_not_leak_a_connection_slot() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(5), async {
            let (cdp, _browser_socket) = test_cdp().await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let mut session = test_session(vec![("placeholder", cdp)]);
            let inner = Arc::get_mut(&mut session).unwrap();
            inner.endpoint = endpoint(&format!("http://{address}")).unwrap();
            inner.pages.get_mut().unwrap().clear();
            let browser = Arc::new(Browser::new(std::env::current_dir().unwrap()).unwrap());
            *browser.session.lock().await = Some(session.clone());
            let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut http, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let n = http.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&chunk[..n]);
                    assert!(request.len() <= 4096);
                }
                assert!(String::from_utf8_lossy(&request).contains("/json/list"));
                observed_tx.send(()).unwrap();
                release_rx.await.unwrap();
                let _ = http
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]",
                    )
                    .await;
            });
            let request = tokio::spawn({
                let browser = browser.clone();
                async move {
                    browser
                        .perform(
                            "ssmcp.browser.evaluate",
                            json!({"target_id":"a","expression":"1"}),
                        )
                        .await
                }
            });
            observed_rx.await.unwrap();
            assert_eq!(session.pages.lock().unwrap().len(), 1);
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            assert!(session.pages.lock().unwrap().is_empty());
            assert!(!session.closed.load(Ordering::Acquire));
            release_tx.send(()).unwrap();
            server.await.unwrap();
            browser.disconnect().await;
        })
        .await
        .expect("cancelled discovery did not release its owned connection slot");
    }

    #[tokio::test]
    async fn queued_request_cannot_move_to_a_replacement_session() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (old_cdp, mut old_socket) = test_cdp().await;
            let (new_cdp, mut new_socket) = test_cdp().await;
            let browser = test_browser(vec![("a", old_cdp)]).await;
            let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let old_server = tokio::spawn(async move {
                let request = test_request(&mut old_socket).await;
                assert_eq!(request["params"]["expression"], "'in-flight'");
                observed_tx.send(()).unwrap();
                release_rx.await.unwrap();
                test_reply(&mut old_socket, &request, json!({"result":{"value":"old"}})).await;
                // Disconnect closes the old socket; it must not send the queued action.
                let message = old_socket.next().await.unwrap().unwrap();
                assert!(matches!(message, Message::Close(_)), "{message:?}");
            });
            let new_server = tokio::spawn(async move {
                let request = test_request(&mut new_socket).await;
                assert_eq!(request["params"]["expression"], "'new-session'");
                test_reply(&mut new_socket, &request, json!({"result":{"value":"new"}})).await;
            });
            let active = tokio::spawn({
                let browser = browser.clone();
                async move {
                    browser
                        .perform(
                            "ssmcp.browser.evaluate",
                            json!({"target_id":"a","expression":"'in-flight'"}),
                        )
                        .await
                }
            });
            observed_rx.await.unwrap();
            let queued = browser.perform(
                "ssmcp.browser.evaluate",
                json!({"target_id":"a","expression":"'must-not-run'"}),
            );
            tokio::pin!(queued);
            assert!(poll_once(queued.as_mut()).await.is_pending());
            let disconnect = browser.disconnect();
            tokio::pin!(disconnect);
            assert!(poll_once(disconnect.as_mut()).await.is_pending());
            assert!(browser.session.lock().await.is_none());
            // Simulate attach publishing the newly verified connection while
            // old input/output is still draining. The target ID is reused.
            *browser.session.lock().await = Some(test_session(vec![("a", new_cdp)]));
            let new = browser
                .perform(
                    "ssmcp.browser.evaluate",
                    json!({"target_id":"a","expression":"'new-session'"}),
                )
                .await
                .unwrap();
            assert_eq!(new["structuredContent"]["value"], "new");
            release_tx.send(()).unwrap();
            active.await.unwrap().unwrap();
            assert!(queued.await.unwrap_err().contains("disconnected"));
            disconnect.await;
            old_server.await.unwrap();
            new_server.await.unwrap();
            browser.disconnect().await;
        })
        .await
        .expect("old-session cleanup blocked a replacement or failed to drain");
    }

    #[test]
    fn endpoints_reject_remote_hosts_redirect_origins_and_credentials() {
        for text in [
            "http://example.com:9222",
            "http://localhost:9222",
            "https://127.0.0.1:9222",
            "http://user:pw@127.0.0.1:9222",
            "http://127.0.0.1:9222/path",
            "http://127.0.0.1:9222/?x=1",
            "http://127.0.0.1:9222/#x",
            "http://0.0.0.0:9222",
        ] {
            assert!(endpoint(text).is_err(), "{text}");
        }
        for text in ["http://127.0.0.1:9222", "http://[::1]:9222"] {
            let ep = endpoint(text).unwrap();
            let ws = text.replacen("http:", "ws:", 1) + "/devtools/browser/abc";
            assert!(websocket(&ep, &ws).is_ok());
            assert!(websocket(&ep, "ws://127.0.0.1:9223/devtools/browser/abc").is_err());
            assert!(websocket(&ep, "ws://example.com:9222/devtools/browser/abc").is_err());
        }
    }

    #[test]
    fn arguments_are_strict_and_key_chords_remain_bounded() {
        assert!(parse::<Launch>(json!({"extra":true})).is_err());
        assert!(
            parse::<Launch>(json!({"width":0}))
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(parse::<Target>(json!({"target_id":"x","extra":true})).is_err());
        assert!(valid_target("../json/version").is_err());
        assert!(valid_url("javascript:alert(1)").is_err());
        assert_eq!(
            modifier_bits(&["Control".into(), "Shift".into()]).unwrap(),
            10
        );
        assert!(modifier_bits(&["Control".into(), "Control".into()]).is_err());
        assert_eq!(key_spec("A").unwrap().0, 65);
        assert_eq!(key_spec("界").unwrap().2, Some("界".into()));
        assert!(key_spec("arbitrary-function").is_err());
    }

    #[test]
    fn screenshots_require_valid_jpeg_dimensions() {
        assert_eq!(
            jpeg_dimensions(&[0xff, 0xd8, 0xff, 0xc0, 0, 8, 8, 0, 100, 0, 200, 1]),
            Some((200, 100))
        );
        assert!(jpeg_dimensions(b"not an image").is_none());
        assert!(jpeg_dimensions(&[0xff, 0xd8, 0xff, 0xc0, 0, 20, 8, 0, 100, 0, 200]).is_none());
    }

    #[tokio::test]
    async fn cdp_preserves_errors_and_skips_events() {
        use tokio_tungstenite::accept_async;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let request = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "Browser.getVersion");
            ws.send(Message::Text(
                json!({"method":"Target.targetCreated","params":{}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                json!({"id":request["id"],"error":{"code":-32000,"message":"native failure"}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        });
        let cdp = Cdp::connect(&format!("ws://127.0.0.1:{port}/devtools/browser/test"))
            .await
            .unwrap();
        let error = cdp.call("Browser.getVersion", json!({})).await.unwrap_err();
        assert!(error.contains("-32000") && error.contains("native failure"));
        task.await.unwrap();
    }

    #[test]
    fn tool_names_and_schemas_are_native_and_unique() {
        let tools = Browser::tools();
        let mut names = std::collections::BTreeSet::new();
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert!(name.starts_with("ssmcp.browser."));
            assert!(names.insert(name.to_owned()));
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
        assert_eq!(names.len(), 17);
        assert!(names.contains("ssmcp.browser.discover"));
        // The default must never open a desktop window implicitly.
        assert!(parse::<Launch>(json!({})).unwrap().headless);
        assert!(!parse::<Launch>(json!({"headless":false})).unwrap().headless);
        let launch = Browser::tools()
            .into_iter()
            .find(|tool| tool["name"] == "ssmcp.browser.launch")
            .unwrap();
        assert_eq!(
            launch["inputSchema"]["properties"]["headless"]["default"],
            true
        );
    }
    #[tokio::test]
    async fn cancelled_input_still_sends_release() {
        use tokio::sync::oneshot;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (pressed_tx, pressed_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let focus = test_request(&mut ws).await;
            assert_eq!(focus["method"], "Emulation.setFocusEmulationEnabled");
            assert_eq!(focus["params"]["enabled"], true);
            test_reply(&mut ws, &focus, json!({})).await;
            let down: Value =
                serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap())
                    .unwrap();
            assert_eq!(down["params"]["type"], "keyDown");
            pressed_tx.send(()).unwrap();
            continue_rx.await.unwrap();
            ws.send(Message::Text(
                json!({"id":down["id"],"result":{}}).to_string().into(),
            ))
            .await
            .unwrap();
            let up: Value =
                serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap())
                    .unwrap();
            assert_eq!(up["params"]["type"], "keyUp");
            ws.send(Message::Text(
                json!({"id":up["id"],"result":{}}).to_string().into(),
            ))
            .await
            .unwrap();
            let restore = test_request(&mut ws).await;
            assert_eq!(restore["method"], "Emulation.setFocusEmulationEnabled");
            assert_eq!(restore["params"]["enabled"], false);
            test_reply(&mut ws, &restore, json!({})).await;
        });
        let cdp = Cdp::connect(&format!("ws://{address}/devtools/page/test"))
            .await
            .unwrap();
        let browser = test_browser(vec![("test", cdp)]).await;
        let request = tokio::spawn({
            let browser = browser.clone();
            async move {
                browser
                    .perform("ssmcp.browser.key", json!({"target_id":"test","key":"A"}))
                    .await
            }
        });
        pressed_rx.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        let disconnect = browser.disconnect();
        tokio::pin!(disconnect);
        assert!(poll_once(disconnect.as_mut()).await.is_pending());
        continue_tx.send(()).unwrap();
        // Poll disconnect while the peer waits for focus restoration. Merely
        // constructing the future does not drive its cleanup after input drains.
        tokio::time::timeout(Duration::from_secs(3), async {
            let (server, ()) = tokio::join!(server, disconnect);
            server.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_press_still_sends_release_and_preserves_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let press: Value =
                serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap())
                    .unwrap();
            assert_eq!(press["params"]["type"], "mousePressed");
            ws.send(Message::Text(
                json!({"id":press["id"],"error":{"code":-32000,"message":"press uncertain"}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let release: Value =
                serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap())
                    .unwrap();
            assert_eq!(release["params"]["type"], "mouseReleased");
            ws.send(Message::Text(
                json!({"id":release["id"],"result":{}}).to_string().into(),
            ))
            .await
            .unwrap();
        });
        let cdp = Cdp::connect(&format!("ws://{address}/devtools/page/test"))
            .await
            .unwrap();
        let error = cdp
            .input_pair(
                "Input.dispatchMouseEvent",
                json!({"type":"mousePressed"}),
                json!({"type":"mouseReleased"}),
            )
            .await
            .unwrap_err();
        assert!(error.contains("press uncertain"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn status_probes_the_live_debugger_and_reports_its_exit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let ws_url = format!("ws://{address}/devtools/browser/test");
        let response_url = ws_url.clone();
        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for status in ["200 OK", "503 Service Unavailable"] {
                let (mut http, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let n = http.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "discovery request ended before its headers");
                    request.extend_from_slice(&chunk[..n]);
                    assert!(request.len() <= 4096, "discovery request headers too large");
                }
                assert!(String::from_utf8_lossy(&request).contains("/json/version"));
                let body = json!({"webSocketDebuggerUrl":response_url}).to_string();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                http.write_all(response.as_bytes()).await.unwrap();
            }
            close_rx.await.unwrap();
        });
        let cdp = Cdp::connect(&ws_url).await.unwrap();
        let browser = Browser::new(std::env::current_dir().unwrap()).unwrap();
        *browser.session.lock().await = Some(Arc::new(Session {
            endpoint: endpoint(&format!("http://{address}")).unwrap(),
            browser: cdp,
            pages: StdMutex::new(HashMap::new()),
            owned: StdMutex::new(None),
            version: json!({}),
            closed: AtomicBool::new(false),
        }));
        let ready = browser
            .perform("ssmcp.browser.status", json!({}))
            .await
            .unwrap();
        assert_eq!(ready["structuredContent"]["connected"], true, "{ready}");
        let unavailable = browser
            .perform("ssmcp.browser.status", json!({}))
            .await
            .unwrap();
        assert_eq!(
            unavailable["structuredContent"]["connected"], false,
            "{unavailable}"
        );
        assert!(
            unavailable["structuredContent"]["reason"]
                .as_str()
                .is_some_and(
                    |reason| reason.starts_with("CDP discovery failed:") && reason.contains("503")
                ),
            "the explicit HTTP 503 must report discovery failure: {unavailable}"
        );
        close_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the owned discovery listener did not close")
            .unwrap();
        let gone = browser
            .perform("ssmcp.browser.status", json!({}))
            .await
            .unwrap();
        let state = &gone["structuredContent"];
        assert_eq!(state["connected"], false, "{gone}");
        assert_eq!(
            state["endpoint"], ready["structuredContent"]["endpoint"],
            "{gone}"
        );
        assert_eq!(state["owned"], false, "{gone}");
        assert_eq!(state["native"], true, "{gone}");
        assert_eq!(state["protocol"], "cdp", "{gone}");
        assert!(
            state["reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("CDP discovery failed:")
                    || reason == "browser debugger status probe timed out"),
            "closed debugger must report a discovery error or its bounded probe timeout: {gone}"
        );
        assert!(browser.session.lock().await.is_some(), "{gone}");
        browser.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_launch_removes_profile_after_owned_process_exit() {
        use std::os::unix::fs::PermissionsExt;
        let fixtures = create_profile().unwrap();
        let marker = fixtures.0.join("profile-path");
        let fake = fixtures.0.join("browser");
        let script = format!(
            "#!/bin/sh\nfor arg in \"$@\"; do case \"$arg\" in --user-data-dir=*) printf '%s\\n' \"$arg\" | cut -c17- > '{}';; esac; done\nexec sleep 60\n",
            marker.display()
        );
        std::fs::write(&fake, script).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let browser = Arc::new(Browser::new(std::env::current_dir().unwrap()).unwrap());
        let task = tokio::spawn({
            let browser = browser.clone();
            async move {
                browser
                    .perform("ssmcp.browser.launch", json!({"executable":fake}))
                    .await
            }
        });
        let profile = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(path) = tokio::fs::read_to_string(&marker).await
                    && !path.trim().is_empty()
                {
                    break PathBuf::from(path.trim());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(profile.is_dir());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(10), async {
            while profile.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(browser.session.lock().await.is_none());
        browser.shutdown().await;
    }

    #[tokio::test]
    async fn final_shutdown_rejects_new_sessions_but_disconnect_is_reusable() {
        let browser = Browser::new(std::env::current_dir().unwrap()).unwrap();
        browser
            .perform("ssmcp.browser.disconnect", json!({}))
            .await
            .unwrap();
        assert!(
            browser
                .perform("ssmcp.browser.status", json!({}))
                .await
                .is_ok()
        );
        browser.shutdown().await;
        assert!(
            browser
                .perform("ssmcp.browser.launch", json!({}))
                .await
                .unwrap_err()
                .contains("closed")
        );
    }

    #[test]
    fn chinese_snapshot_keeps_inline_refs_with_an_exact_mcp_byte_budget() {
        let elements: Vec<_> = (1..=100)
            .map(|i| {
                json!({"ref":format!("s1e{i}"),
            "name":"中文按钮".repeat(100),"value":"输入".repeat(500),"role":"button"})
            })
            .collect();
        let info = bound_snapshot(
            json!({"snapshot_id":"s1","target_id":"test","url":"https://example.test/",
            "title":"中文页面","text":"测试文本".repeat(12000),"elements":elements,
            "text_truncated":false,"elements_truncated":false}),
        );
        assert!(result(info.clone()).to_string().len() <= SNAPSHOT_INLINE_LIMIT);
        assert_eq!(info["text_truncated"], true);
        assert_eq!(info["elements_truncated"], true);
        let elements = info["elements"].as_array().unwrap();
        assert!(!elements.is_empty());
        for (index, element) in elements.iter().enumerate() {
            assert_eq!(element["ref"], format!("s1e{}", index + 1));
        }
    }
    #[test]
    fn multiline_snapshot_preserves_inline_refs_through_output_guard() {
        let info = bound_snapshot(json!({
            "snapshot_id":"s1","target_id":"test","url":"https://example.test/",
            "title":"Multiline page","text":"1\n".repeat(600),
            "elements":[{"ref":"s1e1","tag":"button","name":"\n".repeat(600)}],
            "text_truncated":false,"elements_truncated":false
        }));
        assert_eq!(info["text_truncated"], true);
        assert_eq!(info["metadata_truncated"], true);
        assert_eq!(info["elements"][0]["ref"], "s1e1");
        let complete = result(info);
        let output = crate::output::OutputStore::new_connection().unwrap();
        assert_eq!(output.guard_result(complete.clone()), complete);
    }
}
