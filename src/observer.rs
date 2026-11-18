//! Reusable external terminal views for connection-owned command execution.
//! The viewer receives bytes from the actual PTY; it never runs a second command.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::Notify, task::JoinHandle};

const TAIL_BYTES: usize = 128 * 1024;
const MAX_WINDOWS: usize = 48;
const VIEW_TIMEOUT: Duration = Duration::from_secs(8);
const HEARTBEAT: Duration = Duration::from_secs(6);
const IDLE_WINDOW: Duration = Duration::from_secs(180);
static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

struct View {
    bytes: VecDeque<u8>,
    end: u64,
    seen: Option<Instant>,
    launched: Instant,
    process: Option<Child>,
    retired: bool,
    last_used: Instant,
    rows: u16,
    cols: u16,
}
struct Slot {
    id: usize,
    view: Mutex<View>,
    changed: Notify,
}
impl Slot {
    fn append(&self, bytes: &[u8]) {
        let mut view = self.view.lock().unwrap();
        let tail = &bytes[bytes.len().saturating_sub(TAIL_BYTES)..];
        let excess = (view.bytes.len() + tail.len()).saturating_sub(TAIL_BYTES);
        view.bytes.drain(..excess);
        view.bytes.extend(tail);
        view.end += bytes.len() as u64;
        drop(view);
        self.changed.notify_waiters();
    }
    fn connected(&self) -> bool {
        self.view
            .lock()
            .unwrap()
            .seen
            .is_some_and(|seen| seen.elapsed() < HEARTBEAT)
    }
    fn dimensions(&self) -> (u16, u16) {
        let view = self.view.lock().unwrap();
        (view.rows, view.cols)
    }
}
struct State {
    slots: Vec<Arc<Slot>>,
    active: HashMap<String, usize>,
    provider: HashMap<String, String>,
    streamed: HashMap<String, usize>,
    processes: HashMap<String, String>,
    backgrounds: HashMap<String, String>,
    detached: HashMap<String, Lease>,
}
pub(crate) struct Pool {
    terminal: PathBuf,
    allocation: tokio::sync::Mutex<()>,
    helper: PathBuf,
    directory: PathBuf,
    state: Mutex<State>,
    closed: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}
pub(crate) struct Lease {
    pool: Weak<Pool>,
    key: String,
    slot: Arc<Slot>,
    finished: bool,
}
impl Lease {
    pub(crate) fn slot_id(&self) -> usize {
        self.slot.id
    }
    pub(crate) fn append(&self, bytes: &[u8]) {
        self.slot.append(bytes);
    }
    pub(crate) fn dimensions(&self) -> (u16, u16) {
        self.slot.dimensions()
    }
    pub(crate) fn finish(mut self, status: &str) {
        self.slot
            .append(format!("\r\n\x1b[0;90m[{status}]\x1b[0m\r\n").as_bytes());
        self.release();
    }
    fn release(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(pool) = self.pool.upgrade() {
            let mut state = pool.state.lock().unwrap();
            state.active.remove(&self.key);
            state.provider.remove(&self.key);
            state.streamed.remove(&self.key);
            state.processes.retain(|_, key| key != &self.key);
            state.backgrounds.retain(|_, key| key != &self.key);
        }
        self.slot.view.lock().unwrap().last_used = Instant::now();
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.finished {
            self.slot
                .append(b"\r\n\x1b[0;90m[execution ended]\x1b[0m\r\n");
            self.release();
        }
    }
}

impl Pool {
    pub(crate) async fn new(terminal: &str) -> crate::Result<Arc<Self>> {
        validate_terminal(terminal)?;
        let terminal = if Path::new(terminal).components().count() > 1 {
            let path = Path::new(terminal).canonicalize()?;
            if !crate::platform::is_executable(&path) {
                return Err("--tty requires an executable file".into());
            }
            path
        } else {
            crate::program::executable(terminal)?
        };
        let mut nonce = [0u8; 8];
        crate::platform::random_bytes(&mut nonce)?;
        let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory =
            std::env::temp_dir().join(format!("ssmcp-tty-{}-{suffix}", std::process::id()));
        crate::platform::private_directory(&directory)?;
        let server = match crate::session::ipc::Server::bind(&directory, "view.json").await {
            Ok(server) => server,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        let pool = Arc::new(Self {
            terminal,
            allocation: tokio::sync::Mutex::new(()),
            helper: std::env::current_exe()?,
            directory,
            state: Mutex::new(State {
                slots: Vec::new(),
                active: HashMap::new(),
                provider: HashMap::new(),
                streamed: HashMap::new(),
                processes: HashMap::new(),
                backgrounds: HashMap::new(),
                detached: HashMap::new(),
            }),
            closed: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&pool);
        let server = Arc::new(server);
        let task = tokio::spawn(async move {
            let capacity = Arc::new(tokio::sync::Semaphore::new(MAX_WINDOWS + 4));
            loop {
                let peer = match server.accept().await {
                    Ok(peer) => peer,
                    Err(_) => break,
                };
                let Ok(permit) = capacity.clone().try_acquire_owned() else {
                    continue;
                };
                let weak = weak.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let mut peer = peer;
                    let response = match peer.read().await {
                        Ok(request) => match weak.upgrade() {
                            Some(pool) => pool.read_view(request).await,
                            None => Ok(json!({"closed":true})),
                        },
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = peer.reply(response).await;
                });
            }
        });
        pool.tasks.lock().unwrap().push(task);
        let slot = pool.add_slot()?;
        slot.append(b"\x1b[0;90mssmcp \xc2\xb7 waiting for commands\x1b[0m\r\n");
        pool.launch(&slot)?;
        pool.wait_for_view(&slot).await?;
        let weak = Arc::downgrade(&pool);
        pool.tasks.lock().unwrap().push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(pool) = weak.upgrade() else { break };
                if pool.closed.load(Ordering::Acquire) {
                    break;
                }
                pool.retire_idle();
                let slots = pool.state.lock().unwrap().slots.clone();
                for slot in slots {
                    let reopen = {
                        let mut view = slot.view.lock().unwrap();
                        if let Some(child) = view.process.as_mut() {
                            let _ = child.try_wait();
                        }
                        !view.retired
                            && view.launched.elapsed() > HEARTBEAT
                            && !view.seen.is_some_and(|seen| seen.elapsed() < HEARTBEAT)
                    };
                    if reopen && let Err(error) = pool.launch(&slot) {
                        eprintln!("ssmcp: command observer could not reopen: {error}");
                    }
                }
            }
        }));
        Ok(pool)
    }
    fn retire_idle(&self) {
        let state = self.state.lock().unwrap();
        for slot in &state.slots {
            if slot.id == 1 || state.active.values().any(|id| *id == slot.id) {
                continue;
            }
            let mut view = slot.view.lock().unwrap();
            if view.retired || view.last_used.elapsed() < IDLE_WINDOW {
                continue;
            }
            view.retired = true;
            view.seen = None;
            view.bytes.clear();
            view.bytes.shrink_to_fit();
            view.end = 0;
            if let Some(mut child) = view.process.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            drop(view);
            slot.changed.notify_waiters();
        }
    }
    fn add_slot(&self) -> Result<Arc<Slot>, String> {
        let mut state = self.state.lock().unwrap();
        if state.slots.len() >= MAX_WINDOWS {
            return Err("command observer window limit reached".into());
        }
        let slot = Arc::new(Slot {
            id: state.slots.len() + 1,
            view: Mutex::new(View {
                bytes: VecDeque::new(),
                end: 0,
                seen: None,
                launched: Instant::now(),
                process: None,
                retired: false,
                last_used: Instant::now(),
                rows: 30,
                cols: 100,
            }),
            changed: Notify::new(),
        });
        state.slots.push(slot.clone());
        Ok(slot)
    }
    fn launch(&self, slot: &Slot) -> Result<(), String> {
        let mut view = slot.view.lock().unwrap();
        if !view.retired && view.process.is_some() && view.launched.elapsed() < HEARTBEAT {
            return Ok(());
        }
        if let Some(mut child) = view.process.take() {
            // The launcher may have handed off to an existing terminal server.
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let args = launcher_args(&self.terminal, &self.helper, &self.directory, slot.id);
        let child = Command::new(&self.terminal)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not open {}: {error}", self.terminal.display()))?;
        view.process = Some(child);
        view.launched = Instant::now();
        view.seen = None;
        view.retired = false;
        view.last_used = Instant::now();
        Ok(())
    }
    async fn wait_for_view(&self, slot: &Slot) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + VIEW_TIMEOUT;
        loop {
            let notified = slot.changed.notified();
            if slot.connected() {
                return Ok(());
            }
            {
                let mut view = slot.view.lock().unwrap();
                if let Some(child) = view.process.as_mut()
                    && let Some(status) = child.try_wait().map_err(|error| error.to_string())?
                    && !status.success()
                {
                    return Err(format!("--tty launcher exited with {status}"));
                }
            }
            tokio::select! {
                _ = notified => {},
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "--tty {} did not open its observer within {} seconds; check DISPLAY/Wayland and the terminal's execute-command option",
                    self.terminal.display(),
                    VIEW_TIMEOUT.as_secs()
                ));
            }
        }
    }
    pub(crate) async fn begin(
        self: &Arc<Self>,
        key: String,
        command: &Value,
        cwd: &Path,
    ) -> Result<Lease, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("command observer is closed".into());
        }
        let allocation = self.allocation.lock().await;
        let available = {
            let state = self.state.lock().unwrap();
            state
                .slots
                .iter()
                .find(|slot| !state.active.values().any(|id| *id == slot.id))
                .cloned()
        };
        let slot = match available {
            Some(slot) => slot,
            None => {
                let slot = self.add_slot()?;
                self.launch(&slot)?;
                slot
            }
        };
        {
            let mut state = self.state.lock().unwrap();
            state.active.insert(key.clone(), slot.id);
        }
        drop(allocation);
        let lease = Lease {
            pool: Arc::downgrade(self),
            key,
            slot: slot.clone(),
            finished: false,
        };
        if !slot.connected() {
            let needs_launch = {
                let view = slot.view.lock().unwrap();
                view.retired || view.process.is_none() || view.launched.elapsed() > HEARTBEAT
            };
            if needs_launch {
                self.launch(&slot)?
            }
            self.wait_for_view(&slot).await?;
        }
        slot.append(
            format!(
                "\r\n\x1b[0;36m{}\x1b[0m\r\n$ {}\r\n",
                visible(&cwd.to_string_lossy()),
                display_command(command)
            )
            .as_bytes(),
        );
        Ok(lease)
    }
    async fn read_view(&self, request: Value) -> Result<Value, String> {
        let id = request["slot"].as_u64().ok_or("missing observer slot")? as usize;
        let cursor = request["cursor"].as_u64().unwrap_or(0);
        let slot = self
            .state
            .lock()
            .unwrap()
            .slots
            .iter()
            .find(|slot| slot.id == id)
            .cloned()
            .ok_or("unknown observer slot")?;
        if self.closed.load(Ordering::Acquire) {
            return Ok(json!({"closed":true}));
        }
        let empty = {
            let mut view = slot.view.lock().unwrap();
            if view.retired {
                return Ok(json!({"closed":true}));
            }
            view.seen = Some(Instant::now());
            view.rows = request["rows"].as_u64().unwrap_or(30).clamp(1, 500) as u16;
            view.cols = request["cols"].as_u64().unwrap_or(100).clamp(1, 1000) as u16;
            cursor == view.end
        };
        slot.changed.notify_waiters();
        let notified = slot.changed.notified();
        if empty {
            let _ = tokio::time::timeout(Duration::from_millis(1000), notified).await;
        }
        let view = slot.view.lock().unwrap();
        if view.retired {
            return Ok(json!({"closed":true}));
        }
        if cursor > view.end {
            return Err("observer cursor is ahead".into());
        }
        let start = view.end - view.bytes.len() as u64;
        let offset = cursor.max(start);
        let bytes: Vec<u8> = view
            .bytes
            .iter()
            .skip((offset - start) as usize)
            .take(32768)
            .copied()
            .collect();
        Ok(
            json!({"data_base64":STANDARD.encode(&bytes),"next_cursor":offset+bytes.len() as u64,
            "truncated":cursor<start,"closed":self.closed.load(Ordering::Acquire)}),
        )
    }
    pub(crate) async fn terminal_begin(
        self: &Arc<Self>,
        id: &str,
        cwd: &Path,
    ) -> Result<(), String> {
        let key = format!("terminal:{id}");
        if self.state.lock().unwrap().active.contains_key(&key) {
            return Ok(());
        }
        let mut lease = self
            .begin(key, &json!(format!("interactive terminal {id}")), cwd)
            .await?;
        // Interactive shells own their slot until the shell closes.
        lease.finished = true;
        Ok(())
    }
    pub(crate) fn terminal_output(&self, id: &str, bytes: &[u8]) {
        self.append_key(&format!("terminal:{id}"), bytes);
    }
    pub(crate) fn terminal_closed(&self, id: &str) {
        let key = format!("terminal:{id}");
        self.append_key(&key, b"\r\n[terminal closed]\r\n");
        let slot = {
            let mut state = self.state.lock().unwrap();
            let id = state.active.remove(&key);
            state.streamed.remove(&key);
            id.and_then(|id| state.slots.iter().find(|slot| slot.id == id).cloned())
        };
        if let Some(slot) = slot {
            slot.view.lock().unwrap().last_used = Instant::now();
        }
    }
    fn append_key(&self, key: &str, bytes: &[u8]) {
        let slot = {
            let mut state = self.state.lock().unwrap();
            let Some(id) = state.active.get(key).copied() else {
                return;
            };
            *state.streamed.entry(key.into()).or_default() += bytes.len();
            state.slots.iter().find(|slot| slot.id == id).cloned()
        };
        if let Some(slot) = slot {
            slot.append(bytes)
        }
    }
    pub(crate) async fn native_begin(
        self: &Arc<Self>,
        provider: &str,
        name: &str,
        args: &Value,
    ) -> Result<Option<Lease>, String> {
        let Some(command) = native_command(name, args) else {
            return Ok(None);
        };
        let key = crate::backend::rpc::HarnessCall::current_id()
            .map(|id| format!("native:{id}"))
            .unwrap_or_else(|| {
                format!("native-local:{}", NEXT_CALL.fetch_add(1, Ordering::Relaxed))
            });
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let lease = self.begin(key.clone(), command, &cwd).await?;
        self.state
            .lock()
            .unwrap()
            .provider
            .insert(key, provider.into());
        Ok(Some(lease))
    }
    pub(crate) fn map_background(&self, path: &Path) {
        if let Some(call_id) = crate::backend::rpc::HarnessCall::current_id() {
            let key = format!("native:{call_id}");
            let mut state = self.state.lock().unwrap();
            if state.active.contains_key(&key) {
                state
                    .backgrounds
                    .insert(path.to_string_lossy().into_owned(), key);
            }
        }
    }
    pub(crate) fn native_event(&self, provider: &str, event: &Value) {
        if let Some(path) = event["path"].as_str()
            && event["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("background_output"))
        {
            let key = self.state.lock().unwrap().backgrounds.get(path).cloned();
            if let Some(key) = key {
                if let Some(encoded) = event["data_base64"].as_str()
                    && let Ok(bytes) = STANDARD.decode(encoded)
                {
                    self.append_key(&key, &bytes);
                }
                if let Some(error) = event["error"].as_str() {
                    self.append_key(
                        &key,
                        format!("\r\n[background output error: {error}]\r\n").as_bytes(),
                    );
                }
                if event["type"] == "background_output_closed"
                    || event["type"] == "background_output_error"
                {
                    let lease = {
                        let mut state = self.state.lock().unwrap();
                        state.backgrounds.remove(path);
                        if state.backgrounds.values().any(|other| other == &key) {
                            None
                        } else {
                            state.detached.remove(&key)
                        }
                    };
                    if let Some(lease) = lease {
                        lease.finish("background output source closed");
                    }
                }
            }
            return;
        }
        if provider == "shell" {
            return;
        }
        if event["type"] == "ssmcp.request"
            && let Some(call_id) = event["harness_call_id"].as_u64()
        {
            let request = &event["request"];
            let process = request
                .pointer("/params/processId")
                .or_else(|| request.get("id"));
            if let Some(process) = process.filter(|value| value.is_string() || value.is_number()) {
                let key = format!("native:{call_id}");
                let mut state = self.state.lock().unwrap();
                if state.active.contains_key(&key) {
                    state
                        .processes
                        .insert(format!("{provider}:{}", process), key);
                }
            }
        }
        let bytes = if let Some(data) = event.pointer("/params/deltaBase64").and_then(Value::as_str)
        {
            STANDARD.decode(data).ok()
        } else if event["type"] == "bash_execution_update" {
            event
                .get("output")
                .or_else(|| event.get("chunk"))
                .and_then(Value::as_str)
                .map(|s| s.as_bytes().to_vec())
        } else {
            None
        };
        let Some(bytes) = bytes else { return };
        let key = event["harness_call_id"]
            .as_u64()
            .map(|id| format!("native:{id}"))
            .or_else(|| {
                let process = event
                    .pointer("/params/processId")
                    .or_else(|| event.get("id"))?;
                self.state
                    .lock()
                    .unwrap()
                    .processes
                    .get(&format!("{provider}:{}", process))
                    .cloned()
            })
            .or_else(|| {
                let state = self.state.lock().unwrap();
                let mut keys = state
                    .provider
                    .iter()
                    .filter(|(_, label)| label.as_str() == provider)
                    .map(|(key, _)| key);
                let first = keys.next()?;
                keys.next().is_none().then(|| first.clone())
            });
        if let Some(key) = key {
            self.append_key(&key, &bytes)
        }
    }
    pub(crate) fn native_finish(&self, lease: Lease, result: &Result<Value, String>) {
        let streamed = self
            .state
            .lock()
            .unwrap()
            .streamed
            .get(&lease.key)
            .copied()
            .unwrap_or(0);
        match result {
            Ok(value) => {
                let backgrounds = crate::backend::cc::background_paths(value);
                if !backgrounds.is_empty() {
                    lease.append(b"\r\n[background command; watching its native output]\r\n");
                    let mut state = self.state.lock().unwrap();
                    for path in backgrounds {
                        state
                            .backgrounds
                            .insert(path.to_string_lossy().into_owned(), lease.key.clone());
                    }
                    state.detached.insert(lease.key.clone(), lease);
                    return;
                }
                if streamed == 0 {
                    if let Some(content) = value.get("content").and_then(Value::as_array) {
                        for part in content {
                            if let Some(text) = part["text"].as_str() {
                                lease.append(text.as_bytes());
                                lease.append(b"\r\n")
                            }
                        }
                    } else {
                        lease.append(value.to_string().as_bytes())
                    }
                }
                lease.finish(if value["isError"] == true {
                    "provider reported an error"
                } else {
                    "provider call completed"
                });
            }
            Err(error) => {
                lease.append(error.as_bytes());
                lease.finish("provider call failed");
            }
        }
    }
    pub(crate) fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let detached = {
            let mut state = self.state.lock().unwrap();
            for slot in &state.slots {
                slot.changed.notify_waiters();
            }
            std::mem::take(&mut state.detached)
        };
        drop(detached);
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        for task in self.tasks.get_mut().unwrap().drain(..) {
            task.abort()
        }
        for slot in &self.state.get_mut().unwrap().slots {
            if let Some(mut child) = slot.view.lock().unwrap().process.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
pub(crate) fn validate_terminal(terminal: &str) -> Result<(), String> {
    if terminal.is_empty() || terminal.len() > 4096 || terminal.chars().any(char::is_control) {
        return Err("--tty requires a terminal executable name or path".into());
    }
    Ok(())
}
fn launcher_args(
    terminal: &Path,
    helper: &Path,
    directory: &Path,
    slot: usize,
) -> Vec<std::ffi::OsString> {
    let name = terminal
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let mut args: Vec<std::ffi::OsString> = match name.as_str() {
        "kitty" => vec![
            "--title".into(),
            format!("ssmcp · commands {slot}").into(),
            "--".into(),
        ],
        "gnome-terminal" | "kgx" | "foot" | "footclient" => vec!["--".into()],
        "wezterm" => vec!["start".into(), "--".into()],
        "wt" => vec!["new-tab".into(), "--".into()],
        _ => vec!["-e".into()],
    };
    args.extend([
        helper.as_os_str().to_owned(),
        "__tty-view".into(),
        directory.as_os_str().to_owned(),
        slot.to_string().into(),
    ]);
    args
}
fn visible(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch == '\n' || ch == '\t' || !ch.is_control() {
                vec![ch]
            } else {
                ch.escape_default().collect()
            }
        })
        .collect()
}
fn quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || "_./:=,@%+-".contains(c))
    {
        text.into()
    } else {
        format!("'{}'", visible(text).replace('\'', "'\\''"))
    }
}
fn display_command(command: &Value) -> String {
    command
        .as_array()
        .map(|args| {
            args.iter()
                .map(|arg| quote(arg.as_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| visible(command.as_str().unwrap_or("command")))
}
fn native_command<'a>(name: &str, args: &'a Value) -> Option<&'a Value> {
    let identity = [
        Some(name),
        args["method"].as_str(),
        args["type"].as_str(),
        args["operation_id"].as_str(),
        args["url_path"].as_str(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();
    if !["bash", "shell", "exec", "command", "powershell", "run"]
        .iter()
        .any(|part| identity.contains(part))
    {
        return None;
    }
    let command = |object: &'a Value| {
        ["command", "cmd", "script"]
            .iter()
            .find_map(|key| object.get(*key))
            .filter(|value| value.is_string() || value.is_array())
    };
    command(args).or_else(|| {
        ["params", "args", "body", "arguments"]
            .iter()
            .find_map(|key| args.get(*key).and_then(command))
    })
}
pub(crate) async fn viewer(directory: &Path, slot: usize) -> crate::Result<()> {
    let mut cursor = 0;
    loop {
        let (rows, cols) = viewer_size();
        let response = tokio::select! {
            value=crate::session::ipc::request(directory,"view.json",json!({"slot":slot,"cursor":cursor,"rows":rows,"cols":cols}))=>value?,
            _=tokio::signal::ctrl_c()=>return Ok(()),
        };
        if response["closed"] == true {
            return Ok(());
        }
        let bytes = STANDARD.decode(
            response["data_base64"]
                .as_str()
                .ok_or("observer output missing")?,
        )?;
        let mut stdout = io::stdout().lock();
        if response["truncated"] == true {
            stdout.write_all(b"\r\n[older output retained in agent output artifacts]\r\n")?
        }
        stdout.write_all(&bytes)?;
        stdout.flush()?;
        cursor = response["next_cursor"]
            .as_u64()
            .ok_or("observer cursor missing")?;
    }
}
fn viewer_size() -> (u16, u16) {
    #[cfg(unix)]
    {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: stdout is borrowed, size is initialized writable storage.
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_row > 0
            && size.ws_col > 0
        {
            return (size.ws_row, size.ws_col);
        }
    }
    (30, 100)
}
#[cfg(unix)]
pub(crate) mod pty {
    use super::{Lease, Slot};
    use std::{
        io,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        pin::Pin,
        process::Stdio,
        sync::Arc,
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::{
        io::{AsyncRead, ReadBuf, unix::AsyncFd},
        task::JoinHandle,
    };

    pub(crate) struct Output {
        master: Arc<AsyncFd<OwnedFd>>,
        resize: JoinHandle<()>,
    }
    impl Output {
        pub(crate) fn open(lease: &Lease) -> io::Result<(Self, Stdio, Stdio)> {
            let (rows, cols) = lease.dimensions();
            let mut size = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let (mut master, mut slave) = (-1, -1);
            // SAFETY: openpty writes two fresh descriptors into initialized locals.
            if unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful openpty returned exclusively owned descriptors.
            let (master, slave) =
                unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
            for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
                // SAFETY: both descriptors are live and newly created.
                if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            // SAFETY: master remains owned until after all reactor registrations.
            if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let master = Arc::new(unsafe { AsyncFd::register(master) }?);
            let resize_fd = master.clone();
            let slot = Arc::downgrade(&lease.slot);
            let resize = tokio::spawn(async move {
                let mut previous = (rows, cols);
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let Some(slot) = slot.upgrade() else { break };
                    let dimensions = Slot::dimensions(&slot);
                    if dimensions != previous {
                        let size = libc::winsize {
                            ws_row: dimensions.0,
                            ws_col: dimensions.1,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        // SAFETY: resize_fd owns master throughout this ioctl.
                        unsafe {
                            libc::ioctl(resize_fd.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &size)
                        };
                        previous = dimensions;
                    }
                }
            });
            let stdout = Stdio::from(slave.try_clone()?);
            Ok((Self { master, resize }, stdout, Stdio::from(slave)))
        }
    }
    impl AsyncRead for Output {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            loop {
                let mut ready = std::task::ready!(self.master.poll_read_ready(cx))?;
                let result = ready.try_io(|fd| {
                    let target = buf.initialize_unfilled();
                    // SAFETY: target is writable, bounded storage and fd is borrowed.
                    let count = unsafe {
                        libc::read(
                            fd.get_ref().as_raw_fd(),
                            target.as_mut_ptr().cast(),
                            target.len(),
                        )
                    };
                    if count >= 0 {
                        Ok(count as usize)
                    } else {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() == Some(libc::EIO) {
                            Ok(0)
                        } else {
                            Err(error)
                        }
                    }
                });
                match result {
                    Ok(Ok(count)) => {
                        buf.advance(count);
                        return Poll::Ready(Ok(()));
                    }
                    Ok(Err(error)) => return Poll::Ready(Err(error)),
                    Err(_) => continue,
                }
            }
        }
    }
    impl Drop for Output {
        fn drop(&mut self) {
            self.resize.abort()
        }
    }
}

#[cfg(test)]
mod tests;
